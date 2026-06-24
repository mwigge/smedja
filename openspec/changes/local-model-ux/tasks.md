## 1. Local model inventory (adapter)

- [x] 1.1 Add a failing test in `crates/smedja-adapter/src/local.rs` asserting `parse_model_inventory` returns every `data[].id` from a multi-model `/v1/models` body (not just the first)
- [x] 1.2 Add `LocalModel { id, est_vram_mb: Option<u64> }` and replace `LocalCapability.model_id: String` with `inventory: Vec<LocalModel>` plus `active_model_id: Option<String>`; update `health_check` to populate them
- [x] 1.3 Update the existing `connect`/health-check tests for the inventory shape; keep the unhealthy-when-no-server and default-endpoint cases green
- [x] 1.4 Export the new types from `crates/smedja-adapter/src/lib.rs`
- [x] 1.5 Run `cargo test -p smedja-adapter` — green

## 2. GPU detection probe (adapter)

- [x] 2.1 Add a failing test asserting `parse_gpu_snapshot` extracts `device`, `vram_total_mb`, `vram_free_mb` from captured `nvidia-smi --query-gpu` CSV output
- [x] 2.2 Add a failing test asserting a no-GPU / tool-absent path yields an explicit `GpuSnapshot::none()` (not an error)
- [x] 2.3 Implement the `gpu` probe module: shell-out to the vendor tool, defensive parse, cached `GpuSnapshot`; export the type
- [x] 2.4 Add `fit_for(model, snapshot) -> Fit { Fits, Tight, Exceeds, Unknown }` with a test covering each branch (including `Unknown` when `est_vram_mb` is `None`)
- [x] 2.5 Run `cargo test -p smedja-adapter` — green

## 3. Hot-swap path (adapter)

- [x] 3.1 Add a failing test asserting `LocalProvider::swap_model(id)` issues a swap request to `SMEDJA_LOCAL_SWAP_ENDPOINT` and, on success, reports the new active model id
- [x] 3.2 Add a failing test asserting the fallback path: when the explicit swap endpoint is unsupported, `swap_model` sets the active-model label so subsequent `stream_chat` requests carry the chosen `model`
- [x] 3.3 Implement `swap_model` and the swap-proxy client; update `active_model_id` on success
- [x] 3.4 Run `cargo test -p smedja-adapter` — green

## 4. Pool wiring (smdjad)

- [x] 4.1 Add a failing test in `bin/smdjad/src/provider_pool.rs` asserting the `local` pool entry exposes its full inventory and a mutable `active_model_id`
- [x] 4.2 Update `build_provider_pool` to register the `local` runner with its inventory and active model; place `active_model_id` behind the entry lock
- [x] 4.3 Run `cargo test -p smdjad` — green

## 5. local.* RPC handlers (smdjad)

- [x] 5.1 Add a failing test for `local.models` returning the GPU-annotated inventory (each entry carries `fit`)
- [x] 5.2 Add a failing test for `local.gpu` returning the cached `GpuSnapshot` and an explicit no-GPU shape
- [x] 5.3 Add a failing test for `local.swap { model }` updating the active model with no provider rebuild, and reporting swap latency
- [x] 5.4 Add a failing test for `local.install { model }` reporting success only when the model afterwards appears in the inventory, and a structured error when local tooling is unavailable
- [x] 5.5 Implement the four handlers in `bin/smdjad/src/handlers/` and register them in `bin/smdjad/src/main.rs` alongside `runner.list`
- [x] 5.6 Run `cargo test -p smdjad` — green

## 6. smj local subcommand (CLI)

- [x] 6.1 Add a failing test asserting `smj local list` renders the inventory with fit annotations and `smj local swap <model>` calls `local.swap`
- [x] 6.2 Add the `Local` subcommand to `bin/smj/src/main.rs` (`install`, `list`, `gpu`, `swap`) dispatching to the `local.*` RPCs
- [x] 6.3 Run `cargo test -p smj` — green

## 7. /model picker + swap UX (TUI)

- [x] 7.1 Add a failing test asserting that when the active runner is `local`, `/model <name>` dispatches `local.swap` (not the relabel-only `session.set_model`)
- [x] 7.2 Add a failing test asserting `/model` with no args, for the `local` runner, lists the GPU-annotated inventory via `local.models`
- [x] 7.3 Implement the local-aware `/model` branch and the install/swap slash completions in `bin/smedja-tui/src/main.rs`; keep alphabetical `SLASH_COMPLETIONS` ordering
- [x] 7.4 Run `cargo test -p smedja-tui` — green

## 8. Observability

- [x] 8.1 Emit an OTel span `smedja.local.swap` (attributes: `smedja.local.from_model`, `smedja.local.to_model`, swap latency) and `smedja.local.install` (attribute: `smedja.local.model_id`, result)
- [x] 8.2 Add a `smedja_local_swaps_total` counter labelled by result (ok|error), mirroring the existing `smedja_local_health_checks_total`

## 9. Verify

- [x] 9.1 Run `cargo test --workspace` — all green
- [x] 9.2 Run `cargo clippy -p smedja-adapter -p smdjad -p smj -p smedja-tui -- -D warnings` — clean for the touched code
- [x] 9.3 Update the README local-model section to reflect the install / inventory / GPU / swap surface and the rs-llmctl / llama-swap dependency
- [x] 9.4 Run `openspec validate local-model-ux --strict` — clean
