# smedja

A Rust-native agentic platform and GPU-accelerated terminal suite.

## What it is

smedja is a Cargo workspace of three binaries and their supporting crate library:

| Binary | Role |
|---|---|
| `smedja` | Terminal client — ratatui TUI, turn-block UI, modular status bar |
| `smdjad` | Daemon — Tokio supervision tree, runner orchestration, UDS JSON-RPC 2.0 |
| `smj` | Control CLI — lifecycle management, session control, workspace tools |
| `smedja-term` | GPU terminal emulator — wgpu renderer, block model, native daemon integration |

## Workspace layout

```
crates/
  smedja-rpc         JSON-RPC 2.0 types + UDS codec
  smedja-memory      WorkingMemory, strata compaction, BuildPrompt
  smedja-adapter     Provider HTTP streaming (OpenAI, Gemini, …)
  smedja-assayer     Model routing by role × complexity × tier
  smedja-vault       Vector KV store (cold memory retrieval)
  smedja-ingot       SQLite audit log, cost ledger, session history
  smedja-bellows     Turn lifecycle, event dispatch, pipeline
  smedja-methodology TDD gate, ponytail gate, OpenSpec integration
  smedja-graph       tree-sitter code indexer, graph_query tool
  smedja-sre         SRE agent tools (otel_query, metric_query, log_tail)

bin/
  smedja             Terminal client
  smdjad             Daemon
  smj                Control CLI

term/
  crates/
    st-render        wgpu cell-grid renderer, glyph atlas
    st-pty           PTY spawn + VT100/VT220 emulator
    st-config        TOML config, WezTerm migration adapter
    st-statusbar     Starship-compatible modular status bar
    st-blocks        Block model (shell blocks + agent blocks)
    st-glyph         Glyph Protocol (APC custom glyphs, PUA registry)
    st-agent         smdjad UDS connector, agent-mode rendering
  bin/
    smedja-term      GPU terminal emulator
```

## Naming

Internal metalworking theme (replacing the milliways kitchen theme):

| Concept | Name | Meaning |
|---|---|---|
| Router | assayer | tests and routes by quality |
| Adapters | forge / cast | shapes output to provider spec |
| Storage | ingot | the produced unit / audit record |
| Long-term store | vault | cold, durable storage |
| Context engine | crucible | where material is held under heat |
| Pipeline | bellows | drives throughput |
| Specification | blueprint | the plan to work from |
| Orchestrator | foreman | runs the floor |

## Protocol

smdjad speaks UDS JSON-RPC 2.0 on `$XDG_RUNTIME_DIR/smdjad.sock` (Linux) or
`$TMPDIR/smdjad.sock` (macOS). It also exposes an ACP-compatible HTTP endpoint on
`localhost:7730` (opt-in via `SMEDJA_ACP_PORT`).

Wire protocol is compatible with milliways — a milliways Go client can connect to smdjad
and vice versa during the migration period.

## License

Apache 2.0 — see [LICENSE](LICENSE).
