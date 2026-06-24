//! Loader for the `.smedja/filters.toml` command-output filter DSL.
//!
//! Mirrors [`crate::security::load_security_config`]: a missing file degrades to
//! the built-in default registry ([`FilterRegistry::with_defaults`]); a present
//! file parses `[filters.<cmd>]` entries and merges them *over* the defaults so
//! user entries override built-ins by command key. Longer (two-token) keys win
//! over shorter ones at resolution time inside the registry.
//!
//! The DSL selects a strategy and parameters only — nothing Turing-complete:
//!
//! ```toml
//! [filters.cargo]
//! strategy = "smart-filter"   # smart-filter | group | truncate | dedup | none
//! keep = ["error", "warning"] # smart-filter: line markers to retain
//!
//! [filters."docker build"]    # two-token key wins over one-token "docker"
//! strategy = "truncate"
//! max_lines = 40
//! ```

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use smedja_adapter::{FilterEntry, FilterParams, FilterRegistry, FilterStrategy};

/// Top-level parsed shape of `.smedja/filters.toml`.
#[derive(Debug, Default, Deserialize)]
struct FiltersFile {
    /// Per-command filter table keyed by the command string.
    #[serde(default)]
    filters: HashMap<String, RawFilter>,
}

/// A single parsed `[filters.<cmd>]` table.
#[derive(Debug, Deserialize)]
struct RawFilter {
    /// Strategy DSL name: `smart-filter` | `group` | `truncate` | `dedup` | `none`.
    strategy: String,
    /// Marker substrings for the smart-filter strategy.
    #[serde(default)]
    keep: Vec<String>,
    /// Maximum kept line count for the truncate strategy.
    #[serde(default)]
    max_lines: Option<usize>,
}

/// Loads the filter registry for `workspace_root`.
///
/// Reads `<workspace_root>/.smedja/filters.toml` when present. A missing file,
/// an unparseable one, or an entry naming an unknown strategy degrades to the
/// built-in default registry (the offending entry is skipped with a warning),
/// so command filtering is never disabled by config trouble.
#[must_use]
pub fn load_filter_registry(workspace_root: &Path) -> FilterRegistry {
    let mut registry = FilterRegistry::with_defaults();

    let config_path = workspace_root.join(".smedja").join("filters.toml");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return registry;
    };

    let parsed: FiltersFile = match toml::from_str(&content) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::warn!(
                error = %e,
                path = %config_path.display(),
                "invalid .smedja/filters.toml; using built-in default filters"
            );
            return registry;
        }
    };

    for (command_key, raw) in parsed.filters {
        let Some(strategy) = FilterStrategy::from_str(&raw.strategy) else {
            tracing::warn!(
                command = %command_key,
                strategy = %raw.strategy,
                "unknown filter strategy in .smedja/filters.toml; keeping default for this command"
            );
            continue;
        };
        registry.insert(
            command_key,
            FilterEntry {
                strategy,
                params: FilterParams {
                    keep: raw.keep,
                    max_lines: raw.max_lines,
                },
            },
        );
    }

    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use smedja_adapter::FilterStrategy;

    fn write_filters(dir: &Path, body: &str) {
        let smedja = dir.join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("filters.toml"), body).unwrap();
    }

    #[test]
    fn missing_file_yields_default_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = load_filter_registry(dir.path());
        // Defaults: cargo → smart-filter, git status → group.
        assert_eq!(
            registry.resolve("cargo build").0,
            FilterStrategy::SmartFilter
        );
        assert_eq!(registry.resolve("git status").0, FilterStrategy::Group);
    }

    #[test]
    fn present_file_parses_strategy_and_params() {
        let dir = tempfile::tempdir().unwrap();
        write_filters(
            dir.path(),
            "[filters.\"docker build\"]\nstrategy = \"truncate\"\nmax_lines = 12\n",
        );
        let registry = load_filter_registry(dir.path());
        let (strategy, params) = registry.resolve("docker build -t img .");
        assert_eq!(strategy, FilterStrategy::Truncate);
        assert_eq!(params.max_lines, Some(12));
    }

    #[test]
    fn user_entry_overrides_default_for_same_key() {
        let dir = tempfile::tempdir().unwrap();
        // cargo defaults to smart-filter; override it to none.
        write_filters(dir.path(), "[filters.cargo]\nstrategy = \"none\"\n");
        let registry = load_filter_registry(dir.path());
        assert_eq!(registry.resolve("cargo build").0, FilterStrategy::None);
    }

    #[test]
    fn user_two_token_key_wins_over_one_token() {
        let dir = tempfile::tempdir().unwrap();
        write_filters(
            dir.path(),
            "[filters.docker]\nstrategy = \"dedup\"\n\n\
             [filters.\"docker build\"]\nstrategy = \"truncate\"\n",
        );
        let registry = load_filter_registry(dir.path());
        assert_eq!(
            registry.resolve("docker build .").0,
            FilterStrategy::Truncate,
            "two-token user key must win over one-token user key"
        );
        assert_eq!(registry.resolve("docker ps").0, FilterStrategy::Dedup);
    }

    #[test]
    fn unknown_strategy_keeps_default_for_command() {
        let dir = tempfile::tempdir().unwrap();
        write_filters(dir.path(), "[filters.cargo]\nstrategy = \"bogus\"\n");
        let registry = load_filter_registry(dir.path());
        // The bogus entry is skipped; cargo keeps its default smart-filter.
        assert_eq!(
            registry.resolve("cargo build").0,
            FilterStrategy::SmartFilter
        );
    }

    #[test]
    fn unparseable_file_yields_default_registry() {
        let dir = tempfile::tempdir().unwrap();
        write_filters(dir.path(), "this is not valid toml {{{");
        let registry = load_filter_registry(dir.path());
        assert_eq!(
            registry.resolve("cargo build").0,
            FilterStrategy::SmartFilter
        );
    }
}
