//! `smj shell` — inject precmd/postcmd hooks into shell config files.

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;

/// Which shell to target for hook injection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ShellKind {
    Bash,
    Zsh,
    Fish,
}

#[derive(Subcommand)]
pub(crate) enum ShellCmd {
    /// Manage precmd/postcmd hook injection
    Hook {
        #[command(subcommand)]
        action: HookCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum HookCmd {
    /// Inject smedja shell hooks into the appropriate shell config file.
    ///
    /// Idempotent — running this command twice does not create duplicate entries.
    Install {
        /// Target shell.  Defaults to the value of `$SHELL`.
        #[arg(long, value_enum)]
        shell: Option<ShellKind>,
    },
}

/// Dispatches a `smj shell` subcommand.
pub(crate) fn run(action: ShellCmd) -> Result<()> {
    match action {
        ShellCmd::Hook {
            action: HookCmd::Install { shell },
        } => {
            let kind = shell
                .or_else(detect_shell_kind)
                .ok_or_else(|| anyhow::anyhow!("could not detect shell; pass --shell"))?;
            let config_path = match kind {
                ShellKind::Fish => {
                    let config_home = std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| {
                        std::env::var("HOME")
                            .map_or_else(|_| ".config".into(), |h| format!("{h}/.config"))
                    });
                    PathBuf::from(config_home).join("fish/config.fish")
                }
                ShellKind::Zsh => {
                    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                        .join(".zshrc")
                }
                ShellKind::Bash => {
                    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                        .join(".bashrc")
                }
            };
            hook_install(kind, &config_path)?;
        }
    }
    Ok(())
}

/// Returns the block of shell code that must be injected for `kind`.
///
/// The block is wrapped in a sentinel comment so [`hook_install`] can detect
/// and skip duplicate installs.
#[must_use]
pub(crate) fn hook_content(kind: ShellKind) -> String {
    match kind {
        ShellKind::Bash => {
            // DEBUG trap fires before each command; BASH_COMMAND holds the raw
            // command text.  PROMPT_COMMAND wires the function into the prompt cycle.
            [
                "# smedja-hook-begin",
                "_smj_precmd() { smj session prompt --message \"$BASH_COMMAND\" 2>/dev/null || true; }",
                "PROMPT_COMMAND=\"_smj_precmd;${PROMPT_COMMAND}\"",
                "# smedja-hook-end",
            ]
            .join("\n")
        }
        ShellKind::Zsh => {
            // precmd_functions is the idiomatic zsh hook array; history[$HISTCMD]
            // expands to the last executed command text.
            [
                "# smedja-hook-begin",
                "_smj_precmd() { smj session prompt --message \"${history[$HISTCMD]}\" 2>/dev/null || true; }",
                "precmd_functions+=(_smj_precmd)",
                "# smedja-hook-end",
            ]
            .join("\n")
        }
        ShellKind::Fish => [
            "# smedja-hook-begin",
            "function _smj_postexec --on-event fish_postexec",
            "    smj session prompt --message \"$argv[1]\" 2>/dev/null",
            "end",
            "# smedja-hook-end",
        ]
        .join("\n"),
    }
}

/// Detects `kind` from `$SHELL` when no explicit `--shell` is given.
fn detect_shell_kind() -> Option<ShellKind> {
    let shell = std::env::var("SHELL").ok()?;
    if shell.contains("fish") {
        Some(ShellKind::Fish)
    } else if shell.contains("zsh") {
        Some(ShellKind::Zsh)
    } else {
        Some(ShellKind::Bash)
    }
}

/// Appends the hook block to `path`, unless the sentinel is already present
/// (idempotency guard).
fn hook_install(kind: ShellKind, path: &Path) -> Result<()> {
    use std::io::Write as _;

    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.contains("# smedja-hook-begin") {
        println!("Hook already installed in {}", path.display());
        return Ok(());
    }
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    writeln!(f, "\n{}", hook_content(kind))?;
    println!("Hook installed in {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_content_bash_contains_precmd() {
        let content = hook_content(ShellKind::Bash);
        assert!(
            content.contains("_smj_precmd"),
            "bash hook must define a precmd function: {content}"
        );
        assert!(
            content.contains("PROMPT_COMMAND"),
            "bash hook must wire up PROMPT_COMMAND: {content}"
        );
    }

    #[test]
    fn hook_content_fish_contains_postexec() {
        let content = hook_content(ShellKind::Fish);
        assert!(
            content.contains("fish_postexec"),
            "fish hook must use fish_postexec event: {content}"
        );
    }

    #[test]
    fn hook_install_is_idempotent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join(".bashrc");
        // First install.
        hook_install(ShellKind::Bash, &path).expect("first install");
        let after_first = std::fs::read_to_string(&path).expect("read after first");
        // Second install — sentinel already present, should not duplicate.
        hook_install(ShellKind::Bash, &path).expect("second install");
        let after_second = std::fs::read_to_string(&path).expect("read after second");
        assert_eq!(
            after_first, after_second,
            "second install must not change file content"
        );
    }
}
