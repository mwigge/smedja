## Why

smedja's `local` runner is health-check only. `LocalProvider::connect` (`crates/smedja-adapter/src/local.rs`) reads `SMEDJA_LOCAL_ENDPOINT` (default `http://127.0.0.1:9090`), does one `GET /v1/models` with a 500 ms deadline, parses the first model id, and wraps an `OpenAiProvider`. `build_provider_pool` (`bin/smdjad/src/provider_pool.rs:240`) registers exactly one `(Runner::Local, Tier::Local)` entry from that single probe. There is no install, no model inventory beyond the first id, no GPU awareness, and no way to swap the served local model.

The Go predecessor (milliways) gave local models a real lifecycle: install commands, multi-model serving via a swap proxy, a GPU-aware picker, and sub-second hot-swap between local models. smedja dropped all of it. Today an operator who wants a different local model must stop the daemon, reconfigure the external server by hand, and restart — the `/model` slash command (`bin/smedja-tui/src/main.rs:804`) calls `session.set_model`, which only relabels the session; it cannot make the local server actually serve a different model.

This change adds a local-model management surface — install, inventory, select, GPU detection, and hot-swap — while staying honest about the boundary: smedja **orchestrates** external local-serving tools (rs-llmctl / llama-swap); it does not reimplement an inference server. smedja drives install via documented shell-outs, reads model inventory and GPU facts, and issues swap requests to a llama-swap-style proxy; the heavy lifting (download, quantisation, GPU placement, model load/unload) stays in those external tools.

## What Changes

- **Add a `local-model-management` capability** owning the model inventory, the active-model selection, GPU detection, install orchestration, and the hot-swap path for the `local` runner.
- **Multi-model local serving via a swap proxy**: `LocalProvider` learns the full `/v1/models` inventory (not just the first id) and gains a `swap_model(model_id)` path that issues the swap request to a llama-swap-compatible endpoint, so one `local` runner can serve many models behind one OpenAI-compatible endpoint.
- **GPU-aware model picker**: a GPU detection probe (VRAM total/free, device name) feeds an annotated inventory so the picker can flag which local models fit available VRAM. Detection shells out to vendor tools (`nvidia-smi`, etc.); smedja parses and caches the result, it does not link GPU SDKs.
- **Install orchestration**: a `local install` surface that drives the external installer (rs-llmctl) to pull a model, reporting progress and refusing to claim success unless the model afterwards appears in `/v1/models`. smedja shells out to the installer; it does not download or quantise weights itself.
- **Sub-second hot-swap UX**: extend the `/model` TUI command and add an `smj local` CLI subcommand so an operator can install, list, and swap the active local model, with the swap completing without a daemon restart.
- **New RPCs** (`local.models`, `local.gpu`, `local.swap`, `local.install`) wired into `bin/smdjad`, surfaced by `bin/smj` and `bin/smedja-tui`.

Out of scope (referenced only): reimplementing an inference server; managing remote model hosts; non-local runner model management (owned by the existing `session.set_model` path); the failover rotation ring (owned by `provider-failover`).

## Capabilities

### New Capabilities

- `local-model-management`: smedja maintains a local-model inventory (from the swap proxy's `/v1/models`), a GPU capability snapshot, install orchestration over the external installer, and a hot-swap path that switches the active local model without a daemon restart — surfaced through `local.*` RPCs, the `smj local` subcommand, and the `/model` TUI picker.

## Impact

- `crates/smedja-adapter/src/local.rs`: extend `LocalCapability` to a full model inventory; add `LocalProvider::models`, `swap_model`, and a swap-proxy client; add a GPU-detection probe module.
- `crates/smedja-adapter/src/lib.rs`: export the new inventory / GPU / swap types.
- `bin/smdjad/src/provider_pool.rs`: register the `local` runner with its inventory and current active model; expose the inventory through the pool entry.
- `bin/smdjad/src/handlers/`: add `local.models`, `local.gpu`, `local.swap`, `local.install` RPC handlers; register them in `bin/smdjad/src/main.rs`.
- `bin/smj/src/main.rs`: add a `Local` subcommand (`install`, `list`, `gpu`, `swap`).
- `bin/smedja-tui/src/main.rs`: make `/model` (local runner) drive `local.swap` and the GPU-annotated picker; add the install/swap completions.
- README: the local-model lifecycle claims become accurate and the rs-llmctl / llama-swap dependency is documented.
