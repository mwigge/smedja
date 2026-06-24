## ADDED Requirements

### Requirement: All vault text is embedded through an Embedder port

The daemon SHALL embed all vault-bound text through an `Embedder` port rather than a hard-wired function. The port SHALL expose a vector-producing `embed` operation, a stable `model_id`, and a `dim`. Every call site that previously invoked `crate::embedder::embed` SHALL invoke the resolved port instead.

#### Scenario: call sites use the resolved port

- **WHEN** the daemon embeds a query or stored content on any vault path (cold retrieval, `smedja_vault_search`, lean-specs, compact)
- **THEN** it SHALL call the resolved `Embedder` port's `embed` operation
- **AND** the resulting vector's length SHALL equal the port's reported `dim`

#### Scenario: FNV-1a is the named default backend

- **WHEN** no learned embedder is configured or available
- **THEN** the resolved port SHALL be the FNV-1a backend reporting `model_id = "fnv-bow-128"` and `dim = 128`
- **AND** its `embed` output SHALL be byte-identical to the existing bag-of-words `embed`

### Requirement: Embedder selection is config- and availability-driven

The daemon SHALL resolve exactly one `Embedder` implementation at startup, selected by the `[embedder]` block of `.smedja/config.toml` and by runtime availability. A missing, unparseable, or unavailable configuration SHALL resolve to the FNV-1a default and SHALL NOT block startup.

#### Scenario: learned backend selected when configured and reachable

- **WHEN** `[embedder] backend = "learned"` is set and the learned endpoint passes its startup health check
- **THEN** the resolved port SHALL be the learned backend reporting its configured `model_id` and `dim`

#### Scenario: missing config resolves to the FNV default

- **WHEN** `.smedja/config.toml` is absent or its `[embedder]` block is unparseable
- **THEN** the resolved port SHALL be the FNV-1a default
- **AND** daemon startup SHALL proceed without error
