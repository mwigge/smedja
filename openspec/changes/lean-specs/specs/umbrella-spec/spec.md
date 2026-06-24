## ADDED Requirements

### Requirement: An umbrella holds the durable shared context once

A change MAY be authored as an umbrella that holds the durable trail of thought — idea, intent, and rough direction — across its `proposal.md`, `design.md`, and `tasks.md`. The umbrella SHALL be the single place its shared context is written, so the model reads that context once rather than once per related change.

#### Scenario: umbrella tasks.md lists slices coarsely

- **WHEN** an umbrella's `tasks.md` is authored
- **THEN** it SHALL enumerate the umbrella's slices as coarse `- [ ]` groups
- **AND** it SHALL NOT decompose those slices into granular per-step tasks

### Requirement: Umbrella content is stored as chunked vault entries under an umbrella namespace

The umbrella's design detail SHALL be stored as chunked vault entries under an `umbrella:<id>` namespace via the existing `Vault::insert`/`upsert` path, each entry's `payload` recording the kind discriminator `{"kind":"umbrella","umbrella_id":<id>}`, modelled on the established vault payload convention.

#### Scenario: umbrella detail is chunked into the umbrella namespace

- **WHEN** an umbrella's design detail is stored
- **THEN** each chunk SHALL be a vault entry in the `umbrella:<id>` namespace
- **AND** each entry's `payload` SHALL carry `{"kind":"umbrella","umbrella_id":<id>}`

#### Scenario: an umbrella's chunks are retrievable by id

- **WHEN** a search runs over the `umbrella:<id>` namespace
- **THEN** it SHALL return only that umbrella's chunks
- **AND** entries belonging to other umbrellas or other namespaces SHALL NOT appear in the result
