//! Inline context-fragment expansion for `turn.submit`.
//!
//! Recognises `@file <path>`, `@git`, `@branch`, and `@shell <cmd>` tokens inside
//! a submitted message and expands each in place into a fenced content block,
//! preserving surrounding prose. Expansion runs daemon-side so it can reuse the
//! workspace-boundary check
//! ([`assert_within_workspace`](crate::executor::fs_tools::assert_within_workspace)),
//! the sandboxed shell runner ([`exec_bash`](crate::exec_bash)), and the cowork
//! approval gate ([`CoworkGate`](crate::cowork::CoworkGate)).
//!
//! A fragment is recognised only when `@` begins a token (start-of-string or
//! immediately after whitespace) and `<kind>` is one of the four known kinds, so
//! email addresses and `foo@bar` inside prose are left verbatim. Each fragment's
//! resolved content is size-capped per fragment and per message; over-cap content
//! is truncated with a visible marker.

mod cap;
mod expand;
mod parse;
mod resolve;

// External consumers reach the expander through `crate::fragments::expand`.
pub(crate) use expand::expand;

/// Default per-fragment byte cap (64 KiB), overridable via `SMEDJA_FRAGMENT_MAX_BYTES`.
const DEFAULT_FRAGMENT_MAX_BYTES: usize = 64 * 1024;
/// Default per-fragment line cap (2 000 lines).
const DEFAULT_FRAGMENT_MAX_LINES: usize = 2_000;
/// Default per-message aggregate byte cap (256 KiB), overridable via
/// `SMEDJA_FRAGMENT_MAX_TOTAL_BYTES`.
const DEFAULT_MESSAGE_MAX_BYTES: usize = 256 * 1024;

/// A parsed span of the submitted message: either verbatim literal text or one
/// recognised context fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fragment {
    /// Text passed through unchanged (including unrecognised `@word` tokens).
    Literal(String),
    /// `@file <path>` — inject the file's contents.
    File(String),
    /// `@git` — inject `git status --short` and `git diff HEAD`.
    Git,
    /// `@branch` — inject the current branch and upstream.
    Branch,
    /// `@shell <cmd>` — inject the command's captured output.
    Shell(String),
    /// `@clippy` — inject `cargo clippy --message-format=short` output (no
    /// cowork gate; read-only static analysis, never modifies files).
    Clippy,
    /// `@lsp` — inject the current LSP diagnostic snapshot from the daemon's
    /// `LspManager`. Empty when no language servers are running.
    Lsp,
    /// `@paste:{sha8}` — inject content from a temp paste file.
    Paste(String),
}

/// Per-fragment and per-message size caps, with environment overrides resolved at
/// construction time.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Caps {
    pub(crate) per_fragment_bytes: usize,
    pub(crate) per_fragment_lines: usize,
    pub(crate) message_bytes: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Self {
            per_fragment_bytes: DEFAULT_FRAGMENT_MAX_BYTES,
            per_fragment_lines: DEFAULT_FRAGMENT_MAX_LINES,
            message_bytes: DEFAULT_MESSAGE_MAX_BYTES,
        }
    }
}

impl Caps {
    /// Builds the caps, honouring `SMEDJA_FRAGMENT_MAX_BYTES` and
    /// `SMEDJA_FRAGMENT_MAX_TOTAL_BYTES`. A non-numeric or empty value keeps the
    /// default.
    #[must_use]
    pub(crate) fn from_env() -> Self {
        let mut caps = Self::default();
        if let Some(n) = env_usize("SMEDJA_FRAGMENT_MAX_BYTES") {
            caps.per_fragment_bytes = n;
        }
        if let Some(n) = env_usize("SMEDJA_FRAGMENT_MAX_TOTAL_BYTES") {
            caps.message_bytes = n;
        }
        caps
    }
}

/// Reads a `usize` environment override, returning `None` when unset, empty, or
/// not a valid number.
fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Serialises the (single) test that mutates process-global environment
/// variables so it cannot race concurrent env access from sibling test modules.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Builds explicit caps for tests, avoiding any process-wide env mutation.
#[cfg(test)]
pub(crate) fn test_caps(
    per_fragment_bytes: usize,
    per_fragment_lines: usize,
    message_bytes: usize,
) -> Caps {
    Caps {
        per_fragment_bytes,
        per_fragment_lines,
        message_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::Caps;

    #[test]
    fn env_overrides_fragment_cap() {
        // Serialise against any other env-mutating/reading test to avoid the
        // process-global `set_var`/`remove_var` race under multithreaded runs.
        let _env = super::ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: guarded by `ENV_LOCK`; the var is set and removed within this
        // single critical section.
        unsafe { std::env::set_var("SMEDJA_FRAGMENT_MAX_BYTES", "42") };
        let caps = Caps::from_env();
        unsafe { std::env::remove_var("SMEDJA_FRAGMENT_MAX_BYTES") };
        assert_eq!(caps.per_fragment_bytes, 42);
    }
}
