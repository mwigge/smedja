## ADDED Requirements

### Requirement: Learned embeddings via a local /v1/embeddings endpoint

When a learned backend is configured and its endpoint is reachable, the daemon SHALL embed text by issuing `POST {endpoint}/v1/embeddings` and using the returned vector, reusing the existing local-runner / OpenAI-compatible HTTP path. The learned backend's `model_id` and `dim` SHALL come from configuration.

#### Scenario: query embedded via the learned backend when available

- **WHEN** the learned backend is active and a vault path embeds a query
- **THEN** the daemon SHALL request an embedding from the `/v1/embeddings` endpoint with the configured model
- **AND** the returned vector SHALL be used for the cosine comparison
- **AND** the vector's length SHALL equal the backend's configured `dim`

### Requirement: Graceful fallback when the learned model is unavailable

A missing, unconfigured, or unreachable learned embedder SHALL NOT hard-fail any turn, search, or store. When the learned endpoint is unavailable at startup the daemon SHALL resolve to the FNV-1a backend; when it becomes unreachable on the live path the embed call SHALL degrade rather than panic or abort the turn.

#### Scenario: FNV fallback when offline at startup

- **WHEN** `backend = "learned"` is configured but the endpoint fails its startup health check
- **THEN** the resolved port SHALL be the FNV-1a backend
- **AND** vault search SHALL continue to operate with weaker (lexical) recall

#### Scenario: live-path failure does not abort the turn

- **WHEN** the learned endpoint is unreachable or times out during a live embed call
- **THEN** the embed call SHALL fall back to the FNV-1a vector or return an empty result
- **AND** the turn SHALL NOT abort and the daemon SHALL NOT panic
