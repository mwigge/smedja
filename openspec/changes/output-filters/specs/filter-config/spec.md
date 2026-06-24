## ADDED Requirements

### Requirement: .smedja/filters.toml defines per-command filters

The daemon SHALL load a `.smedja/filters.toml` workspace-config file that maps a command key to a filter `strategy` (`smart-filter`, `group`, `truncate`, `dedup`, or `none`) and optional strategy parameters. The loader MUST follow the established `.smedja/` config convention.

#### Scenario: filters.toml entries parsed into strategies

- **WHEN** `.smedja/filters.toml` contains `[filters.kubectl]` with `strategy = "dedup"`
- **THEN** the loaded registry SHALL resolve the `kubectl` command to the `dedup` strategy

#### Scenario: missing filters.toml falls back to defaults

- **WHEN** no `.smedja/filters.toml` file exists in the workspace
- **THEN** the daemon SHALL use the built-in default filter set
- **AND** loading SHALL NOT fail

### Requirement: User entries override the default filter set

User-defined filters in `.smedja/filters.toml` SHALL take precedence over the built-in defaults for the same command key. A longer (two-token) key SHALL win over a shorter (one-token) key when both match a command.

#### Scenario: user entry overrides a default

- **WHEN** the default set filters `cargo` with `smart-filter` and `.smedja/filters.toml` sets `[filters.cargo] strategy = "none"`
- **THEN** the resolved strategy for a `cargo` command SHALL be `none`

#### Scenario: two-token key wins over one-token key

- **WHEN** `.smedja/filters.toml` defines both `[filters.docker]` and `[filters."docker build"]`
- **THEN** a `docker build` command SHALL resolve to the `docker build` entry
- **AND** a `docker ps` command SHALL resolve to the `docker` entry

### Requirement: Built-in default filter set

The daemon SHALL ship a built-in default filter set covering the highest-volume noisy commands — at least `git`, `cargo`, `pytest`, `npm`, `docker`, and `kubectl` — and SHALL resolve any command outside the set to the conservative `none` fallback.

#### Scenario: defaults cover the common commands

- **WHEN** no user config is present
- **THEN** `git`, `cargo`, `pytest`, `npm`, `docker`, and `kubectl` SHALL each resolve to a built-in strategy
- **AND** an unrecognised command SHALL resolve to `none`
