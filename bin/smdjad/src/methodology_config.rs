//! Daemon-side loader for the foundational-discipline `[methodology]` config.
//!
//! Resolves the `[methodology]` block from `<workspace>/.smedja/config.toml`,
//! mirroring [`crate::security::load_security_config`]: a missing or unparseable
//! file resolves to the all-on default and never blocks startup. The resolved
//! [`MethodologyConfig`] gates both the always-on steering directive and the diff
//! backstop for each discipline.

use std::path::Path;

use smedja_methodology::MethodologyConfig;

/// Resolves the [`MethodologyConfig`] for `workspace_root`.
///
/// Reads `<workspace_root>/.smedja/config.toml` when present. A missing file or
/// an unparseable one resolves to the foundational default (`tdd = true`,
/// `clean = true`), so the discipline is never silently dropped because of config
/// trouble.
#[must_use]
pub fn load_methodology_config(workspace_root: &Path) -> MethodologyConfig {
    let config_path = workspace_root.join(".smedja").join("config.toml");
    let Ok(content) = std::fs::read_to_string(&config_path) else {
        return MethodologyConfig::default();
    };
    match MethodologyConfig::from_toml_str(&content) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!(error = %e, path = %config_path.display(), "invalid [methodology] config; using foundational default");
            MethodologyConfig::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_resolves_to_all_on_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_methodology_config(dir.path());
        assert!(cfg.tdd);
        assert!(cfg.clean);
    }

    #[test]
    fn unparseable_config_resolves_to_all_on_default() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        // Malformed TOML must not block startup; it falls back to the default.
        std::fs::write(smedja.join("config.toml"), "[methodology\ntdd = ").unwrap();
        let cfg = load_methodology_config(dir.path());
        assert!(cfg.tdd);
        assert!(cfg.clean);
    }

    #[test]
    fn tdd_false_block_is_read() {
        let dir = tempfile::tempdir().unwrap();
        let smedja = dir.path().join(".smedja");
        std::fs::create_dir_all(&smedja).unwrap();
        std::fs::write(smedja.join("config.toml"), "[methodology]\ntdd = false\n").unwrap();
        let cfg = load_methodology_config(dir.path());
        assert!(!cfg.tdd);
        assert!(cfg.clean);
    }
}
