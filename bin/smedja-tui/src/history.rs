/// Maximum number of entries kept in the prompt history ring.
pub(crate) const PROMPT_HISTORY_CAP: usize = 500;

/// Returns the canonical path for the TUI-specific prompt-history JSONL file:
/// `~/.config/smedja/tui-history.jsonl`.
pub(crate) fn dirs_tui_history_path() -> std::path::PathBuf {
    let config_dir = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        std::path::PathBuf::from(xdg)
    } else {
        dirs_home().join(".config")
    };
    config_dir.join("smedja").join("tui-history.jsonl")
}

/// Returns the user's home directory from `$HOME`, falling back to `/tmp`.
pub(crate) fn dirs_home() -> std::path::PathBuf {
    if let Ok(home) = std::env::var("HOME") {
        std::path::PathBuf::from(home)
    } else {
        std::path::PathBuf::from("/tmp")
    }
}

/// Maximum entries kept in the persisted TUI history file.
///
/// Matches [`PROMPT_HISTORY_CAP`] so the file and in-memory ring stay in sync.
pub(crate) const HISTORY_FILE_CAP: usize = 500;

/// Serialises `history` to `path` as JSONL (one JSON string per line).
///
/// Only the last [`HISTORY_FILE_CAP`] entries are written so the file stays
/// bounded.  Best-effort — errors are returned to the caller.
pub(crate) fn save_history(history: &[String], path: &std::path::Path) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    let start = history.len().saturating_sub(HISTORY_FILE_CAP);
    for entry in &history[start..] {
        let line = serde_json::to_string(entry)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

/// Loads history from a JSONL file at `path`.
///
/// Returns an empty `Vec` when the file does not exist or cannot be parsed;
/// individual malformed lines are silently skipped.
#[must_use]
pub(crate) fn load_history(path: &std::path::Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<String>(line).ok())
        .collect()
}

pub(crate) const LARGE_PASTE_THRESHOLD: usize = 1024;

/// When `text` is larger than [`LARGE_PASTE_THRESHOLD`] bytes, writes it to a
/// temp file and returns the `@paste:{sha8}` token together with a status
/// message.  When `text` is small, returns `None` (caller keeps text as-is).
pub(crate) fn large_paste_token(text: &str) -> Option<(String, String)> {
    use sha2::Digest as _;
    use std::fmt::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if text.len() <= LARGE_PASTE_THRESHOLD {
        return None;
    }
    let mut hasher = sha2::Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().fold(String::with_capacity(64), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });
    let sha8 = &hex[..8];
    let path = std::path::PathBuf::from(format!("/tmp/smedja-paste-{sha8}.txt"));
    // Write paste file with user-only permissions to prevent info leak.
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // fails if path exists — prevents symlink attack
        .mode(0o600)
        .open(&path)
    {
        let _ = std::io::Write::write_all(&mut f, text.as_bytes());
    }
    let token = format!("@paste:{sha8}");
    let msg = format!("paste saved \u{2192} {token} ({} bytes)", text.len());
    Some((token, msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn prompt_history_capped_at_max_size() {
        let mut history: Vec<String> = Vec::new();
        for i in 0..=PROMPT_HISTORY_CAP {
            history.push(format!("msg{i}"));
            if history.len() > PROMPT_HISTORY_CAP {
                history.remove(0);
            }
        }
        assert_eq!(history.len(), PROMPT_HISTORY_CAP);
    }

    #[test]
    fn save_and_load_history_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        let original = vec!["alpha".to_owned(), "beta".to_owned()];
        save_history(&original, &path).unwrap();
        let loaded = load_history(&path);
        assert_eq!(loaded, original);
    }

    #[test]
    fn load_history_missing_file_returns_empty() {
        let path = std::path::Path::new("/tmp/smedja-test-nonexistent-history-xyz.jsonl");
        let loaded = load_history(path);
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_history_caps_at_file_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("history.jsonl");
        // Write more entries than the cap so the old ones are dropped.
        let entries: Vec<String> = (0..600).map(|i| format!("entry{i}")).collect();
        save_history(&entries, &path).unwrap();
        let loaded = load_history(&path);
        assert_eq!(loaded.len(), HISTORY_FILE_CAP);
        // The last HISTORY_FILE_CAP entries should have been kept.
        let expected_first = format!("entry{}", 600 - HISTORY_FILE_CAP);
        assert_eq!(loaded[0], expected_first);
        assert_eq!(loaded[HISTORY_FILE_CAP - 1], "entry599");
    }

    #[test]
    fn large_paste_placeholder_replaces_input() {
        let big: String = "x".repeat(1025);
        let result = large_paste_token(&big);
        assert!(result.is_some(), "large paste must produce a token");
        let (token, _msg) = result.unwrap();
        assert!(
            token.starts_with("@paste:"),
            "token must start with @paste:"
        );
    }

    #[test]
    fn small_paste_is_kept_verbatim() {
        let small = "hello world".to_owned();
        let result = large_paste_token(&small);
        assert!(result.is_none(), "small paste must return None");
    }
}
