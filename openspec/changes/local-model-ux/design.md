## Context

The `local` runner today is a thin health-checked wrapper:

- `LocalProvider::connect` (`crates/smedja-adapter/src/local.rs`) reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`), runs `health_check` (`GET /v1/models`, 500 ms timeout), and stores a `LocalCapability { model_id, healthy }` holding only the *first* model id. It wraps an `OpenAiProvider` and forwards `stream_chat` unchanged.
- `parse_first_model_id` deliberately reads only `data[0].id` — the rest of the `/v1/models` inventory is discarded.
- `build_provider_pool` (`bin/smdjad/src/provider_pool.rs:240`) calls `connect`, and if healthy adds a single `(Runner::Local, Tier::Local)` entry with `default_model: "local"`. There is no notion of "many local models, one active".
- `runner.list` (`bin/smdjad/src/handlers/session.rs:570`) returns `(runner, tier, model)` triples from `list_all_entries`; the local entry shows one model.
- `/model <name>` in the TUI (`bin/smedja-tui/src/main.rs:804`) calls `session.set_model`, which relabels the session's model string. For a hosted runner that picks a different remote model; for `local` it does nothing useful because the external server is still serving whatever it was started with.

The external tools this change orchestrates:

- **rs-llmctl** — the local control/installer surface (pull/download a model, list installed models). smedja shells out to it; it owns weights on disk.
- **llama-swap** (or a llama-swap-compatible proxy) — fronts many loaded models behind one OpenAI-compatible endpoint and hot-swaps the active model on request (typically by routing on the request `model` field, or an explicit swap/upstream endpoint). smedja issues swap requests to it; it owns model load/unload and GPU placement.

This change adds the management surface around those tools without reimplementing them.

## Goals / Non-Goals

Goals:
- Surface the full local-model inventory (all `/v1/models` entries), not just the first id.
- Let an operator install, list, GPU-inspect, and hot-swap the active local model from `smj local` and the `/model` TUI picker, without restarting the daemon.
- Detect GPU capability (VRAM total/free, device name) and annotate the inventory so the picker can flag which models fit.
- Keep a hard boundary: smedja drives external tools (rs-llmctl, llama-swap) via shell-out / HTTP; it does not download weights, quantise, place models on GPUs, or run inference.

Non-Goals:
- Reimplementing an inference server or a swap proxy in Rust.
- GPU scheduling / placement decisions — llama-swap owns load/unload and VRAM placement; smedja only reports the GPU snapshot and the fit annotation.
- Managing remote (hosted) model selection — that stays on `session.set_model`.
- Multi-host / cluster local serving — single-host localhost only.
- Persisting the active-model choice across daemon restarts beyond what the external proxy already persists.

## Decisions

**Decision: smedja orchestrates; it does not serve.** The capability is a control plane over rs-llmctl (install/inventory) and llama-swap (active-model swap). Install is a shell-out to the installer; swap and inventory are HTTP calls to the proxy; GPU detection is a shell-out to a vendor tool. Inference, downloads, quantisation, and GPU placement stay external.
- Rationale: matches the existing `LocalProvider`-wraps-`OpenAiProvider` boundary and avoids dragging an inference stack into a Rust workspace. Honest scope is what made the milliways security plane regress when over-reached.
- Alternative considered: embed an inference runtime (llama.cpp bindings). Rejected — enormous surface, native build complexity, duplicates llama-swap.

**Decision: inventory is the full `/v1/models` list, cached on the pool entry.** Replace `LocalCapability.model_id: String` with an inventory `Vec<LocalModel>` plus an `active_model_id`. `connect` parses all `data[].id` entries; `parse_first_model_id` becomes `parse_model_inventory`.
- Rationale: the data is already in the response smedja throws away; surfacing it is cheap and unblocks the picker.
- Alternative: a second RPC round-trip per list. Rejected — the health check already fetches `/v1/models`.

**Decision: hot-swap goes through the swap proxy, not a daemon restart.** `LocalProvider::swap_model(id)` issues the swap request to the configured swap endpoint (`SMEDJA_LOCAL_SWAP_ENDPOINT`, defaulting to the same base). On success the proxy serves `id` for subsequent `stream_chat` calls. The pool entry's `active_model_id` is updated in place under a lock; no providers are rebuilt.
- Rationale: a daemon restart loses session state and is the exact friction this change removes; llama-swap is designed for in-place swap.
- Alternative: rebuild the pool entry per swap. Rejected — the provider object and endpoint are unchanged; only the active-model label moves, and rebuilding races concurrent turns.

**Decision: GPU detection shells out and is cached.** A `gpu` probe module shells out to a vendor tool (`nvidia-smi --query-gpu=...` first; absent/zero-GPU returns an explicit "no GPU detected" snapshot, never an error that aborts the daemon). The result (`GpuSnapshot { device, vram_total_mb, vram_free_mb }`) is cached and refreshed on demand via `local.gpu`.
- Rationale: no native GPU SDK linkage; degrades cleanly on CPU-only and non-NVIDIA hosts; the snapshot is advisory for the picker, not a gate.
- Alternative: link a GPU SDK. Rejected — native build burden and vendor lock for an advisory number.

**Decision: model-fit annotation is advisory, computed from the snapshot.** Each `LocalModel` carries an optional `est_vram_mb` (from the installer/inventory metadata where present) and the picker annotates `fits | tight | exceeds | unknown` against `vram_free_mb`. It never blocks a swap — the operator may still select an "exceeds" model and let llama-swap decide.
- Rationale: a picker hint, not a scheduler. Keeps the GPU boundary on the external tool.

**Decision: install refuses to claim success without verification.** `local.install` shells out to the installer, streams progress, and on completion re-queries `/v1/models`; it reports success only if the requested model now appears in the inventory.
- Rationale: avoids the "installed but not actually servable" failure the health-check-only design hides today.

**Decision: surface through `local.*` RPCs, `smj local`, and `/model`.** New RPCs `local.models`, `local.gpu`, `local.swap`, `local.install` (registered in `bin/smdjad/src/main.rs` alongside `runner.list`). `smj local {install|list|gpu|swap}` for scripting; the TUI `/model` picker, when the active runner is `local`, lists the GPU-annotated inventory and drives `local.swap` instead of the relabel-only `session.set_model`.
- Rationale: mirrors the existing `runner.list` / `session.set_model` shapes and the `smj` subcommand-per-domain layout (`Daemon`, `Mcp`, `Sandbox`, `Service`).

## Risks / Trade-offs

- [Risk] External tool absent (no rs-llmctl / llama-swap) → Mitigation: every `local.*` handler returns a structured "local tooling unavailable" error with the install hint; the daemon still starts and other runners are unaffected, matching today's "tier will be skipped" behaviour.
- [Risk] Swap proxy that routes purely on the request `model` field (no explicit swap endpoint) → Mitigation: `swap_model` first attempts the explicit swap endpoint; if unsupported it falls back to setting the active-model label so subsequent `stream_chat` requests carry the chosen `model`, which a model-routing proxy honours.
- [Risk] GPU shell-out parsing drift across tool versions → Mitigation: parse defensively (missing fields → `unknown`), unit-test against captured `nvidia-smi` output, and treat the snapshot as advisory so a parse miss degrades to "unknown fit" rather than a failure.
- [Risk] "Sub-second" swap is a property of llama-swap, not smedja → Mitigation: smedja measures and reports the swap round-trip latency but makes no guarantee beyond issuing the request promptly; the spec asserts the no-restart path, not a hard latency SLA.
- [Risk] Active-model mutation races concurrent turns → Mitigation: `active_model_id` lives behind the pool entry's lock; a swap is applied atomically and in-flight turns keep the model they started with.
- [Risk] Install shell-out is long-running and could block the RPC → Mitigation: install runs as a streaming/async task with progress events; the RPC returns a handle and verification result, never blocking the daemon event loop.
