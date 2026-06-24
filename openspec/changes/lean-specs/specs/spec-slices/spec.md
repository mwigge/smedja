## ADDED Requirements

### Requirement: A slice carries only its own delta and a pointer to its umbrella

A slice SHALL be a thin child spec unit carrying only its own delta and acceptance criteria plus a pointer to its umbrella. The pointer SHALL be metadata — an `umbrella_id` and a `slice_n` — modelled on the vault payload convention, and MUST NOT require a `parent` field in the OpenSpec change manifest, which stays flat (`schema` and `created` only).

#### Scenario: slice records its umbrella pointer as metadata

- **WHEN** a slice is authored
- **THEN** it SHALL record an `umbrella_id` and a `slice_n` as pointer metadata
- **AND** the change manifest `.openspec.yaml` SHALL remain flat, carrying no `parent` field

### Requirement: A slice does not restate its umbrella

A slice MUST NOT restate the umbrella's Why or its design rationale. The slice's own content SHALL contain only the slice-specific delta and acceptance criteria; the umbrella context SHALL be supplied by reference, not by copy.

#### Scenario: slice content excludes the umbrella's Why and design

- **WHEN** a slice's content is assembled
- **THEN** it SHALL NOT contain the umbrella's Why or design rationale text
- **AND** the umbrella context SHALL be present only via the pointer-resolved umbrella, not duplicated inside the slice

### Requirement: A slice resolves its umbrella via the pointer

A slice SHALL resolve its umbrella by its `umbrella_id`, retrieving the umbrella's chunks from the `umbrella:<id>` namespace. A dangling pointer SHALL degrade gracefully rather than fail.

#### Scenario: slice resolves its umbrella by id

- **WHEN** a slice carrying `umbrella_id` resolves its umbrella
- **THEN** the umbrella's chunks SHALL be retrieved from the `umbrella:<id>` namespace
- **AND** the retrieved chunks SHALL be those whose `payload` records the matching `umbrella_id`

#### Scenario: dangling umbrella pointer degrades gracefully

- **WHEN** a slice's `umbrella_id` resolves to a namespace with no stored chunks
- **THEN** resolution SHALL return an empty result rather than an error
- **AND** the slice SHALL proceed on its own delta and acceptance criteria alone
