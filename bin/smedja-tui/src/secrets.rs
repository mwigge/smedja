//! Secrets storage — write API keys to `~/.config/smedja/secrets.env`.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

/// Saves `value` under `var` in `~/.config/smedja/secrets.env`, replacing any
/// existing line for that variable, and chmods the file to 0600.
///
/// Returns a status string that never contains the secret value.
pub(crate) fn save_secret(var: &str, value: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    let path = PathBuf::from(home)
        .join(".config")
        .join("smedja")
        .join("secrets.env");
    save_to_path(var, value, &path)
}

fn save_to_path(var: &str, value: &str, path: &Path) -> String {
    if !valid_env_name(var) {
        return format!("login: invalid environment variable name: {var}");
    }
    if value.contains(['\n', '\r', '\0']) {
        return "login: key must be a single line".to_owned();
    }
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return "login: cannot create ~/.config/smedja".to_owned();
        }
    }
    let prefix = format!("{var}=");
    let mut lines: Vec<String> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.starts_with(&prefix))
        .map(str::to_owned)
        .collect();
    lines.push(format!("{var}={value}"));
    let body = format!("{}\n", lines.join("\n"));
    if std::fs::write(path, body).is_err() {
        return format!("login: failed to write {}", path.display());
    }
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    format!(
        "\u{2713} saved {var} to {} (0600). Activate: {}",
        path.display(),
        activation_hint()
    )
}

fn activation_hint() -> &'static str {
    if cfg!(target_os = "linux") {
        "add\n  EnvironmentFile=%h/.config/smedja/secrets.env\nto the smdjad unit, then: systemctl --user restart smdjad"
    } else if cfg!(target_os = "macos") {
        "restart smdjad with `launchctl kickstart -k gui/$(id -u)/nu.wigge.smedja.smdjad`"
    } else {
        "restart smdjad so it inherits the updated environment"
    }
}

fn valid_env_name(var: &str) -> bool {
    let mut chars = var.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_uppercase())
        && chars.all(|c| c == '_' || c.is_ascii_uppercase() || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_to_path_returns_success_message() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        let result = save_to_path("MY_API_KEY", "test-value-123", &path);
        assert!(result.contains("\u{2713}"), "expected ✓ in: {result}");
        assert!(result.contains("MY_API_KEY"));
        assert!(
            !result.contains("test-value-123"),
            "secret must not appear in output"
        );
    }

    #[test]
    fn save_to_path_replaces_existing_var() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        save_to_path("MY_API_KEY", "old-value", &path);
        save_to_path("MY_API_KEY", "new-value", &path);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("old-value"), "old value must be replaced");
        assert!(content.contains("MY_API_KEY=new-value"));
    }

    #[test]
    fn save_to_path_sets_mode_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        save_to_path("MY_API_KEY", "abc", &path);
        let meta = std::fs::metadata(&path).unwrap();
        let mode = meta.permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file must be 0600");
    }

    #[test]
    fn save_to_path_preserves_other_vars() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        save_to_path("FOO", "foo-val", &path);
        save_to_path("MY_API_KEY", "new-value", &path);
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(
            content.contains("FOO=foo-val"),
            "other vars must be preserved"
        );
    }

    #[test]
    fn save_to_path_rejects_multiline_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        let result = save_to_path("MY_API_KEY", "first\nSECOND=value", &path);
        assert!(
            result.contains("single line"),
            "multiline secret must be rejected: {result}"
        );
        assert!(!path.exists(), "invalid secret must not create the file");
    }

    #[test]
    fn save_to_path_rejects_invalid_var_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets.env");
        let result = save_to_path("bad-name", "value", &path);
        assert!(
            result.contains("invalid environment variable"),
            "bad env var must be rejected: {result}"
        );
        assert!(!path.exists(), "invalid env var must not create the file");
    }
}
