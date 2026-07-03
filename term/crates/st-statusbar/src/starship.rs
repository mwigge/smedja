//! Starship configuration compatibility.

use std::path::Path;

/// Subset of a Starship configuration relevant to the status bar.
#[derive(Debug, Clone)]
pub struct StarshipConfig {
    /// Custom symbol to prefix the branch name (e.g. `" "`).
    pub git_branch_symbol: Option<String>,
    /// Whether the `git_branch` module is disabled in Starship.
    pub git_branch_disabled: bool,
}

/// Attempts to load a [`StarshipConfig`] from a TOML file at `path`.
///
/// Returns `None` if the file does not exist, cannot be read, or cannot be
/// parsed as TOML. All errors are swallowed silently.
pub fn load_starship_fallback(path: &Path) -> Option<StarshipConfig> {
    if !path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&contents).ok()?;

    let git_branch = value.get("git_branch");
    let git_branch_symbol = git_branch
        .and_then(|t| t.get("symbol"))
        .and_then(toml::Value::as_str)
        .map(str::to_owned);
    let git_branch_disabled = git_branch
        .and_then(|t| t.get("disabled"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false);

    Some(StarshipConfig {
        git_branch_symbol,
        git_branch_disabled,
    })
}
