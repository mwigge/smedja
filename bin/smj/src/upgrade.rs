//! `smj upgrade` — check for or install a newer smj release.

use anyhow::Result;

/// Dispatches the `smj upgrade` command.
pub(crate) fn run(check: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    if check {
        eprintln!("smj {current} — upgrade check not yet implemented");
        std::process::exit(1);
    } else {
        eprintln!(
            "smj {current} — self-upgrade not yet implemented; install a new binary from releases"
        );
        std::process::exit(1);
    }
}

/// Returns `true` when `candidate` is strictly newer than `current` using
/// simple version string comparison (`MAJOR.MINOR.PATCH`).
///
/// ponytail: semver ordering by lexicographic component comparison is
/// sufficient for monotonic release numbering; add the `semver` crate if
/// pre-release labels need ordering.
#[must_use]
#[cfg(test)]
fn is_newer(candidate: &str, current: &str) -> bool {
    fn parts(v: &str) -> (u64, u64, u64) {
        let mut it = v.splitn(3, '.').map(|s| s.parse::<u64>().unwrap_or(0));
        (
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
            it.next().unwrap_or(0),
        )
    }
    parts(candidate) > parts(current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser as _;

    #[test]
    fn is_newer_detects_version_bump() {
        assert!(is_newer("0.24.1", "0.24.0"), "patch bump must be newer");
        assert!(is_newer("0.25.0", "0.24.0"), "minor bump must be newer");
        assert!(is_newer("1.0.0", "0.24.0"), "major bump must be newer");
        assert!(!is_newer("0.24.0", "0.24.0"), "equal must not be newer");
        assert!(!is_newer("0.23.9", "0.24.0"), "lower must not be newer");
    }

    #[test]
    fn upgrade_check_only_flag_accepted() {
        let cli =
            Cli::try_parse_from(["smj", "upgrade", "--check"]).expect("upgrade --check must parse");
        match cli.command {
            Cmd::Upgrade { check } => assert!(check, "--check flag must be true"),
            _ => panic!("expected Cmd::Upgrade"),
        }
    }
}
