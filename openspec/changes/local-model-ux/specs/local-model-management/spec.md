## ADDED Requirements

### Requirement: Local model inventory surfaces all served models

The `local` runner SHALL expose the full set of models reported by the swap proxy's `GET /v1/models`, not only the first entry. The provider MUST retain every `data[].id` from the response and an `active_model_id` for the currently served model.

#### Scenario: inventory lists every served model

- **WHEN** the swap proxy's `/v1/models` returns more than one model entry
- **THEN** the local inventory SHALL contain one entry per returned model id
- **AND** the inventory SHALL NOT be truncated to the first entry

#### Scenario: local.models returns the annotated inventory

- **WHEN** a caller invokes the `local.models` RPC
- **THEN** the response SHALL list each local model with its id and a GPU-fit annotation
- **AND** the entry whose id equals the active model SHALL be marked as active

### Requirement: GPU capability is detected and reported

smedja SHALL detect GPU capability by shelling out to a vendor tool and MUST report a `GpuSnapshot` (device name, total VRAM, free VRAM). When no GPU is present or the tool is absent, the snapshot MUST be an explicit "no GPU detected" result rather than an error that aborts the daemon.

#### Scenario: GPU present is reported with VRAM

- **WHEN** a GPU vendor tool is available and reports VRAM figures
- **THEN** `local.gpu` SHALL return the device name, total VRAM, and free VRAM
- **AND** the snapshot SHALL be cached for reuse by the model picker

#### Scenario: no GPU degrades cleanly

- **WHEN** no GPU is present or the vendor tool is absent
- **THEN** `local.gpu` SHALL return an explicit no-GPU snapshot
- **AND** the daemon SHALL continue running with the `local` runner unaffected

### Requirement: Model picker annotates VRAM fit

The local model picker SHALL annotate each model with an advisory fit against the current GPU free VRAM. The annotation MUST be one of `fits`, `tight`, `exceeds`, or `unknown`, and it MUST NOT block a swap.

#### Scenario: model that fits free VRAM is marked fits

- **WHEN** a model's estimated VRAM is known and at most the free VRAM
- **THEN** the picker SHALL annotate the model as `fits`

#### Scenario: unknown estimate yields unknown fit

- **WHEN** a model has no estimated VRAM available
- **THEN** the picker SHALL annotate the model as `unknown`
- **AND** the model SHALL still be selectable for swap

### Requirement: Active local model hot-swaps without daemon restart

The `local` runner SHALL support swapping the active model in place. A swap MUST update the active-model selection so subsequent turns use the new model WITHOUT restarting `smdjad` and WITHOUT rebuilding the provider pool entry.

#### Scenario: swap changes the active model in place

- **WHEN** a caller invokes `local.swap { model }` for a model in the inventory
- **THEN** the active model SHALL become the requested model
- **AND** no daemon restart SHALL be required for subsequent turns to use it

#### Scenario: swap does not disturb in-flight turns

- **WHEN** a swap is applied while a turn is already streaming
- **THEN** the in-flight turn SHALL continue with the model it started on
- **AND** the swap SHALL apply atomically to subsequent turns

#### Scenario: swap falls back to model-routing when no swap endpoint exists

- **WHEN** the swap proxy does not support an explicit swap endpoint
- **THEN** `local.swap` SHALL set the active-model label so subsequent requests carry the chosen `model`
- **AND** the swap SHALL still report success

### Requirement: Local model install is orchestrated and verified

smedja SHALL drive local-model install through the external installer (rs-llmctl) via shell-out, and MUST report success only when the requested model appears in the inventory after install. smedja MUST NOT download or quantise model weights itself.

#### Scenario: install verified against inventory

- **WHEN** `local.install { model }` completes and the model appears in `/v1/models`
- **THEN** the RPC SHALL report success for that model

#### Scenario: install reports failure when model is absent after install

- **WHEN** `local.install { model }` completes but the model is not in `/v1/models`
- **THEN** the RPC SHALL report failure rather than success

#### Scenario: missing local tooling returns a structured error

- **WHEN** the external installer or swap proxy is unavailable
- **THEN** the `local.*` RPC SHALL return a structured "local tooling unavailable" error with an install hint
- **AND** the daemon SHALL remain running

### Requirement: Local model management is reachable from CLI and TUI

The local-model surface SHALL be reachable through both `smj local` and the `/model` TUI picker. When the active runner is `local`, the `/model` command MUST drive `local.swap` rather than the relabel-only `session.set_model` path.

#### Scenario: smj local list shows inventory with fit

- **WHEN** an operator runs `smj local list`
- **THEN** the command SHALL render each local model with its GPU-fit annotation

#### Scenario: TUI /model swaps the local model

- **WHEN** the active runner is `local` and the operator runs `/model <name>`
- **THEN** the TUI SHALL dispatch `local.swap` for the named model
- **AND** the status bar SHALL reflect the newly active local model

### Requirement: Local model operations are observable

Every local-model swap and install SHALL emit an OTel span and update a counter. The swap span MUST carry the source and target model ids and the swap latency.

#### Scenario: swap emits span and counter

- **WHEN** a `local.swap` completes
- **THEN** an OTel span `smedja.local.swap` SHALL be emitted with the from-model, to-model, and latency attributes
- **AND** the `smedja_local_swaps_total` counter SHALL be incremented labelled by result
