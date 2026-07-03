//! Filter registry: strategy selection keyed on the detected command.

use super::filters::{
    dedup_lines, group_by_directory, remove_blank_lines, smart_filter, truncate_lines,
};

/// Default line count kept by the `truncate` strategy when unspecified.
const DEFAULT_TRUNCATE_MAX_LINES: usize = 40;

/// One of the four rtk-style command-output filter strategies, plus the
/// conservative pass-through (`None`, blank-line removal).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FilterStrategy {
    /// Keep only high-signal lines matching the configured markers.
    SmartFilter,
    /// Cluster lines by leading directory with a per-group count.
    Group,
    /// Keep the first N lines and append an omitted-lines marker.
    Truncate,
    /// Collapse runs of identical lines into one with an `(×N)` count.
    Dedup,
    /// Conservative fallback: remove blank lines only.
    None,
}

impl FilterStrategy {
    /// Parses a strategy from its kebab-case DSL name.
    ///
    /// Recognised names: `smart-filter`, `group`, `truncate`, `dedup`, `none`.
    /// Returns `None` for any other input.
    #[must_use]
    #[allow(clippy::should_implement_trait)] // fallible name parse; FromStr's Err type is needless here
    pub fn from_str(name: &str) -> Option<Self> {
        match name {
            "smart-filter" => Some(Self::SmartFilter),
            "group" => Some(Self::Group),
            "truncate" => Some(Self::Truncate),
            "dedup" => Some(Self::Dedup),
            "none" => Some(Self::None),
            _ => None,
        }
    }

    /// Returns the kebab-case DSL name for this strategy.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SmartFilter => "smart-filter",
            Self::Group => "group",
            Self::Truncate => "truncate",
            Self::Dedup => "dedup",
            Self::None => "none",
        }
    }

    /// Applies this strategy to `output` using `params`.
    #[must_use]
    pub fn apply(self, output: &str, params: &FilterParams) -> String {
        match self {
            Self::SmartFilter => smart_filter(output, &params.keep),
            Self::Group => group_by_directory(output),
            Self::Truncate => truncate_lines(
                output,
                params.max_lines.unwrap_or(DEFAULT_TRUNCATE_MAX_LINES),
            ),
            Self::Dedup => dedup_lines(output),
            Self::None => remove_blank_lines(output),
        }
    }
}

/// Parameters for a filter entry.
///
/// `keep` supplies the marker substrings for [`FilterStrategy::SmartFilter`];
/// `max_lines` caps [`FilterStrategy::Truncate`].  Both are ignored by the
/// strategies that do not consume them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilterParams {
    /// Marker substrings retained by the smart-filter strategy.
    pub keep: Vec<String>,
    /// Maximum kept line count for the truncate strategy.
    pub max_lines: Option<usize>,
}

/// One registry entry: the strategy plus its parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterEntry {
    /// The strategy this command resolves to.
    pub strategy: FilterStrategy,
    /// Parameters threaded into [`FilterStrategy::apply`].
    pub params: FilterParams,
}

impl FilterEntry {
    /// Builds an entry from a strategy with default parameters.
    #[must_use]
    pub fn new(strategy: FilterStrategy) -> Self {
        Self {
            strategy,
            params: FilterParams::default(),
        }
    }
}

/// A command-keyed registry mapping a detected command to a [`FilterEntry`].
///
/// Keys are the first one or two whitespace-separated tokens of the trimmed
/// command string (e.g. `cargo`, `git status`, `docker build`).  Longer
/// (two-token) keys win over shorter (one-token) keys, so `docker build` can
/// override the generic `docker` entry.  An unrecognised command resolves to
/// the conservative [`FilterStrategy::None`] (blank-line removal).
#[derive(Debug, Clone, Default)]
pub struct FilterRegistry {
    entries: std::collections::HashMap<String, FilterEntry>,
}

impl FilterRegistry {
    /// Creates an empty registry (every command resolves to `None`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or overrides the entry for `command_key`.
    ///
    /// `command_key` is matched against the leading one or two tokens of a
    /// command at [`Self::resolve`] time.
    pub fn insert(&mut self, command_key: impl Into<String>, entry: FilterEntry) {
        self.entries.insert(command_key.into(), entry);
    }

    /// Resolves `cmd` to a `(strategy, params)` pair.
    ///
    /// Tries the two-token key first (e.g. `git status`), then the one-token key
    /// (e.g. `git`); an unmatched command yields [`FilterStrategy::None`] with
    /// default parameters.
    #[must_use]
    pub fn resolve(&self, cmd: &str) -> (FilterStrategy, FilterParams) {
        let trimmed = cmd.trim();
        let mut tokens = trimmed.split_whitespace();
        let first = tokens.next().unwrap_or("");
        let second = tokens.next();

        if let Some(second) = second {
            let two = format!("{first} {second}");
            if let Some(entry) = self.entries.get(&two) {
                return (entry.strategy, entry.params.clone());
            }
        }
        if let Some(entry) = self.entries.get(first) {
            return (entry.strategy, entry.params.clone());
        }
        (FilterStrategy::None, FilterParams::default())
    }

    /// Builds the built-in default filter set.
    ///
    /// Covers the highest-volume noisy commands: `cargo` and `pytest` →
    /// smart-filter (errors/warnings/failures); `git status` → group (by
    /// directory); `npm`, `docker`, `kubectl` → dedup.  This preserves the
    /// historical `cargo test` and `git status` behaviour as registry entries.
    #[must_use]
    pub fn with_defaults() -> Self {
        let mut registry = Self::new();
        let cargo_keep = vec![
            "error".to_owned(),
            "warning".to_owned(),
            "FAILED".to_owned(),
            "panicked".to_owned(),
        ];
        registry.insert(
            "cargo",
            FilterEntry {
                strategy: FilterStrategy::SmartFilter,
                params: FilterParams {
                    keep: cargo_keep,
                    max_lines: None,
                },
            },
        );
        registry.insert(
            "pytest",
            FilterEntry {
                strategy: FilterStrategy::SmartFilter,
                params: FilterParams {
                    keep: vec![
                        "FAILED".to_owned(),
                        "ERROR".to_owned(),
                        "Error".to_owned(),
                        "assert".to_owned(),
                    ],
                    max_lines: None,
                },
            },
        );
        registry.insert("git status", FilterEntry::new(FilterStrategy::Group));
        registry.insert("npm", FilterEntry::new(FilterStrategy::Dedup));
        registry.insert("docker", FilterEntry::new(FilterStrategy::Dedup));
        registry.insert("kubectl", FilterEntry::new(FilterStrategy::Dedup));
        registry
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_strategy_round_trips_from_name() {
        for name in ["smart-filter", "group", "truncate", "dedup", "none"] {
            let strategy = FilterStrategy::from_str(name)
                .unwrap_or_else(|| panic!("'{name}' must parse to a strategy"));
            assert_eq!(strategy.as_str(), name, "round-trip must be stable");
        }
        assert!(
            FilterStrategy::from_str("bogus").is_none(),
            "unknown names must not parse"
        );
    }

    #[test]
    fn registry_resolves_known_and_unknown_commands() {
        let registry = FilterRegistry::with_defaults();
        assert_eq!(
            registry.resolve("cargo build").0,
            FilterStrategy::SmartFilter
        );
        assert_eq!(registry.resolve("git status").0, FilterStrategy::Group);
        assert_eq!(
            registry.resolve("some-unknown-cmd --flag").0,
            FilterStrategy::None,
            "unknown command must resolve to the conservative None strategy"
        );
    }

    #[test]
    fn registry_two_token_key_wins_over_one_token() {
        let mut registry = FilterRegistry::new();
        registry.insert("docker", FilterEntry::new(FilterStrategy::Dedup));
        registry.insert("docker build", FilterEntry::new(FilterStrategy::Truncate));
        assert_eq!(
            registry.resolve("docker build -t img .").0,
            FilterStrategy::Truncate,
            "two-token key must win"
        );
        assert_eq!(
            registry.resolve("docker ps").0,
            FilterStrategy::Dedup,
            "one-token key applies when no two-token match"
        );
    }

    #[test]
    fn registry_defaults_cover_required_commands() {
        let registry = FilterRegistry::with_defaults();
        assert_ne!(registry.resolve("git status").0, FilterStrategy::None);
        assert_ne!(registry.resolve("cargo build").0, FilterStrategy::None);
        assert_ne!(registry.resolve("pytest -q").0, FilterStrategy::None);
        assert_ne!(registry.resolve("npm install").0, FilterStrategy::None);
        assert_ne!(registry.resolve("docker build .").0, FilterStrategy::None);
        assert_ne!(registry.resolve("kubectl get pods").0, FilterStrategy::None);
    }
}
