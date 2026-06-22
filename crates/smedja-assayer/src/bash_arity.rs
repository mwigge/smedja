//! Shell command arity classifier — read vs write.
//!
//! [`classify_bash`] inspects the first token of each compound-command
//! segment and returns the worst-case [`BashArity`] across all segments.
//! This is used by the `ToolGate` to block write-capable commands when a
//! session role is configured with `bash = ["read"]`.

/// Classifies a bash command as read-only or potentially write-capable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashArity {
    /// Command only reads state; safe for read-only roles.
    Read,
    /// Command may write, delete, or have network side-effects.
    Write,
}

/// Returns the arity of `cmd`.
///
/// Compound commands separated by `;`, `&`, or `|` are split and the worst
/// classification wins — if any segment is [`BashArity::Write`], the whole
/// command is classified as `Write`.
///
/// Output redirection operators (`>` and `>>`) are detected before segment
/// splitting and immediately force a `Write` classification regardless of the
/// command name that precedes them.  This closes the blind spot where
/// `cat foo > /etc/passwd` would otherwise be classified as `Read` because
/// the first token (`cat`) is in the read-only allowlist.
pub fn classify_bash(cmd: &str) -> BashArity {
    if contains_output_redirection(cmd) {
        return BashArity::Write;
    }
    for part in split_compound(cmd) {
        if classify_single(part.trim()) == BashArity::Write {
            return BashArity::Write;
        }
    }
    BashArity::Read
}

/// Returns `true` if `cmd` contains an output redirection operator (`>` or
/// `>>`).
///
/// This check is deliberately conservative: the presence of `>` anywhere in
/// the command string is treated as a potential output redirection.  This
/// avoids false negatives at the cost of possible false positives for commands
/// that include `>` inside quoted strings, but for a security gate that
/// trade-off is correct.
fn contains_output_redirection(cmd: &str) -> bool {
    cmd.contains('>')
}

/// Splits `cmd` on the shell compound-command operators `;`, `|`, `&`, `\n`,
/// and `\r`.
///
/// Newline and carriage-return are included because a multi-line string passed
/// to bash (e.g. via a here-doc or a shell variable) can embed arbitrary
/// commands after a newline — omitting them would allow write-capable commands
/// to escape classification.
///
/// Output redirection operators (`>` / `>>`) are **not** used as delimiters
/// here; they are handled earlier by [`contains_output_redirection`] before
/// this function is called.
///
/// This is intentionally naive — it is sufficient for the guard use-case
/// where the goal is conservative classification, not full shell parsing.
fn split_compound(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split([';', '|', '&', '\n', '\r'])
}

/// Classifies a single (non-compound) command segment.
fn classify_single(part: &str) -> BashArity {
    let mut tokens = part.split_whitespace();
    match tokens.next().unwrap_or("") {
        // Note: `echo` and `printf` are intentionally absent — both can write
        // files via shell redirection (`echo payload > file`). They fall through
        // to the default Write classification.
        "cat" | "ls" | "ll" | "la" | "grep" | "rg" | "find" | "wc" | "head" | "tail" | "pwd"
        | "whoami" | "which" | "type" | "file" | "less" | "more" | "bat" | "fd" | "du" | "df"
        | "stat" | "env" | "jq" | "yq" => BashArity::Read,
        "git" => {
            // Conservative: only known read-only subcommands are safe.
            match tokens.next().unwrap_or("") {
                "log" | "diff" | "status" | "show" | "branch" | "describe" | "rev-parse"
                | "ls-files" | "ls-tree" | "shortlog" => BashArity::Read,
                _ => BashArity::Write,
            }
        }
        "cargo" => match tokens.next().unwrap_or("") {
            "check" | "test" | "clippy" | "doc" | "audit" | "tree" | "metadata" => BashArity::Read,
            _ => BashArity::Write,
        },
        _ => BashArity::Write,
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_bash, BashArity};

    #[test]
    fn cat_is_read() {
        assert_eq!(classify_bash("cat src/foo.rs"), BashArity::Read);
    }

    #[test]
    fn rm_is_write() {
        assert_eq!(classify_bash("rm foo.rs"), BashArity::Write);
    }

    #[test]
    fn git_log_is_read() {
        assert_eq!(classify_bash("git log --oneline -10"), BashArity::Read);
    }

    #[test]
    fn git_commit_is_write() {
        assert_eq!(classify_bash("git commit -m \"wip\""), BashArity::Write);
    }

    #[test]
    fn compound_rm_in_chain_is_write() {
        assert_eq!(classify_bash("ls /tmp && rm -rf /tmp/x"), BashArity::Write);
    }

    // ── smoke-level tests matching task 58 spec ─────────────────────────────

    #[test]
    fn git_rm_is_write_class() {
        assert_eq!(classify_bash("git rm foo.rs"), BashArity::Write);
    }

    #[test]
    fn cargo_test_is_read_class() {
        assert_eq!(classify_bash("cargo test --workspace"), BashArity::Read);
    }

    #[test]
    fn compound_write_classifies_as_write() {
        // Even if first command is read, a write command in the chain → Write.
        assert_eq!(classify_bash("cat foo.rs && rm foo.rs"), BashArity::Write);
    }

    // echo/printf can write files via redirection — must be classified Write.
    #[test]
    fn echo_is_write_class() {
        assert_eq!(classify_bash("echo hello"), BashArity::Write);
    }

    #[test]
    fn printf_is_write_class() {
        assert_eq!(classify_bash("printf '%s\\n' hello"), BashArity::Write);
    }

    // Newline-separated compound commands must also be split and checked.
    #[test]
    fn newline_compound_with_write_is_write() {
        assert_eq!(classify_bash("cat foo.rs\nrm foo.rs"), BashArity::Write);
    }

    #[test]
    fn newline_compound_all_read_is_read() {
        assert_eq!(
            classify_bash("cat foo.rs\ngrep bar foo.rs"),
            BashArity::Read
        );
    }

    // ── output redirection operator tests ───────────────────────────────────

    // `cat` is normally Read, but `>` makes it Write.
    #[test]
    fn cat_with_redirect_is_write() {
        assert_eq!(classify_bash("cat foo > /tmp/out"), BashArity::Write);
    }

    // `ls` is normally Read, but redirecting to /dev/null is still Write.
    #[test]
    fn ls_redirect_to_devnull_is_write() {
        assert_eq!(classify_bash("ls > /dev/null"), BashArity::Write);
    }

    // Append operator `>>` must also be treated as Write.
    #[test]
    fn echo_append_redirect_is_write() {
        assert_eq!(classify_bash("echo hi >> log.txt"), BashArity::Write);
    }

    // Plain `cat` without redirection must remain Read.
    #[test]
    fn cat_without_redirect_is_read() {
        assert_eq!(classify_bash("cat foo"), BashArity::Read);
    }
}
