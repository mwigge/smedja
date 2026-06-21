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
pub fn classify_bash(cmd: &str) -> BashArity {
    for part in split_compound(cmd) {
        if classify_single(part.trim()) == BashArity::Write {
            return BashArity::Write;
        }
    }
    BashArity::Read
}

/// Splits `cmd` on the shell compound-command operators `;`, `|`, and `&`.
///
/// This is intentionally naive — it is sufficient for the guard use-case
/// where the goal is conservative classification, not full shell parsing.
fn split_compound(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split([';', '|', '&'])
}

/// Classifies a single (non-compound) command segment.
fn classify_single(part: &str) -> BashArity {
    let mut tokens = part.split_whitespace();
    match tokens.next().unwrap_or("") {
        "cat" | "ls" | "ll" | "la" | "grep" | "rg" | "find" | "wc" | "head" | "tail" | "echo"
        | "printf" | "pwd" | "whoami" | "which" | "type" | "file" | "less" | "more" | "bat"
        | "fd" | "du" | "df" | "stat" | "env" | "jq" | "yq" => BashArity::Read,
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
}
