## ADDED Requirements

### Requirement: SBOM is assembled from the resolved lockfile

The security plane SHALL assemble a CycloneDX-style SBOM document from the resolved `Cargo.lock`, listing each component's name, version, and package URL (purl). Assembly MUST be offline and MUST NOT invoke `cargo` or any network service. The document MUST record the source lockfile's content hash for traceability.

#### Scenario: components are derived from Cargo.lock

- **WHEN** an SBOM is generated for a workspace with a resolved `Cargo.lock`
- **THEN** the document SHALL contain one component entry per locked package with its name and version
- **AND** the document SHALL include the lockfile content hash

#### Scenario: SBOM generation is non-blocking

- **WHEN** SBOM generation is requested
- **THEN** producing the document SHALL NOT block, refuse, or alter any agent action
- **AND** a missing or unreadable `Cargo.lock` SHALL produce a clear error rather than a partial document

### Requirement: smj security command exposes scan, report, and sbom

The CLI SHALL provide a top-level `smj security` subcommand with `scan`, `report`, and `sbom` actions. `scan` SHALL run a posture scan and print the findings; `report` SHALL summarise recorded `security_finding` audit events; `sbom` SHALL emit the SBOM document.

#### Scenario: sbom action emits the document to stdout

- **WHEN** `smj security sbom` is run in a workspace with a resolved lockfile
- **THEN** the CycloneDX-style SBOM document SHALL be written to standard output
- **AND** the command SHALL exit successfully

#### Scenario: report summarises advisory findings

- **WHEN** `smj security report` is run and advisory `security_finding` events exist
- **THEN** the command SHALL print a summary of those findings including their severity and status
- **AND** the command SHALL run as a read-only query that blocks no action
