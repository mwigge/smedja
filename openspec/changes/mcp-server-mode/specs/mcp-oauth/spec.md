## ADDED Requirements

### Requirement: MCP OAuth uses the Authorization Code + PKCE flow

`start_pkce` SHALL implement the OAuth 2.0 Authorization Code flow with PKCE (S256) and SHALL NOT return `NotImplemented`. It MUST generate a random code verifier, derive the `code_challenge` as `base64url(SHA256(verifier))` with no padding, and never transmit the verifier in the authorization request.

#### Scenario: S256 challenge is derived from the verifier

- **WHEN** a code verifier is generated
- **THEN** the `code_challenge` SHALL equal the unpadded base64url encoding of the SHA-256 digest of the verifier
- **AND** the verifier SHALL NOT appear in the authorization request URL

#### Scenario: flow no longer returns NotImplemented

- **WHEN** `start_pkce` is invoked against a reachable authorization server
- **THEN** it SHALL drive the redirect-and-exchange flow
- **AND** it SHALL NOT return `PkceError::NotImplemented`

### Requirement: PKCE redirect listener validates state on a loopback callback

`start_pkce` SHALL bind a loopback (`127.0.0.1`) redirect listener, accept exactly one callback, and validate the returned `state` against the generated value before accepting the authorization `code`. A mismatched or missing `state` MUST be rejected.

#### Scenario: matching state yields the authorization code

- **WHEN** the redirect callback arrives with a `state` equal to the generated value and a `code`
- **THEN** the listener SHALL extract the `code` and proceed to the token exchange

#### Scenario: mismatched state is rejected

- **WHEN** the redirect callback arrives with a `state` that does not match the generated value
- **THEN** the listener SHALL reject the callback and SHALL NOT proceed to the token exchange

#### Scenario: no callback within the timeout

- **WHEN** no redirect callback arrives within the configured timeout
- **THEN** `start_pkce` SHALL return `PkceError::Cancelled`

### Requirement: tokens are exchanged, persisted, and refreshed

`start_pkce` SHALL exchange the authorization code for a `Token` and persist it via `TokenStore`. When a stored access token has expired and a refresh token is present, the daemon SHALL perform a `refresh_token` grant and re-persist the new token.

#### Scenario: code is exchanged and the token stored

- **WHEN** the authorization code is exchanged at the token endpoint
- **THEN** `start_pkce` SHALL return the parsed `Token`
- **AND** the token SHALL be persisted via `TokenStore::save` keyed by the server URL

#### Scenario: token exchange transport failure maps to Http error

- **WHEN** the token-endpoint request fails at the transport level
- **THEN** `start_pkce` SHALL return `PkceError::Http`

#### Scenario: expired token is refreshed

- **WHEN** a stored token has expired and carries a refresh token
- **THEN** the daemon SHALL perform a `grant_type=refresh_token` exchange
- **AND** the resulting token SHALL be re-persisted via `TokenStore`

### Requirement: outbound MCP calls use stored tokens

When dispatching an MCP tool call or refreshing a server's tool list, the daemon SHALL resolve an access token by loading the stored token for the server URL, falling back to the `MCP_TOKEN` environment variable, then to no token, and SHALL pass the resolved token to the MCP client.

#### Scenario: stored token is sent as the Bearer credential

- **WHEN** a tool is dispatched to a server that has a stored token
- **THEN** the daemon SHALL send the stored token's access token as the Bearer credential

#### Scenario: no stored token falls back to environment then empty

- **WHEN** a tool is dispatched to a server with no stored token
- **THEN** the daemon SHALL use the `MCP_TOKEN` environment variable if set
- **AND** SHALL otherwise send no credential, preserving the unauthenticated path
