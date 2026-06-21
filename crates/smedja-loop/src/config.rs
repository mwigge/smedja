//! Loop engine configuration, loaded from `.smedja/loop.json`.
//!
//! The policy hash is computed at load time so that callers can detect
//! post-load tampering by calling [`LoopConfig::verify_policy`].

use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::role::LoopRole;

/// Top-level loop engine configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopConfig {
    /// Schema version of this configuration file.
    pub version: u32,
    /// Bounded execution limits.
    pub limits: Limits,
    /// Ordered list of roles participating in the loop.
    pub roles: Vec<LoopRole>,
    /// Verification gate configuration.
    pub verification: Verification,
    /// Per-slice review policy.
    pub review: Review,
    /// Publication constraints.
    pub publication: Publication,
    /// SHA-256 hash of the raw JSON bytes, computed at load time.
    ///
    /// This field is excluded from serialisation; it is always derived
    /// from the file content by [`LoopConfig::from_file`].
    #[serde(skip)]
    pub policy_hash: String,
}

/// Bounded execution limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Maximum number of attempts per slice before the loop fails.
    pub max_attempts: u32,
    /// Per-agent call timeout in seconds.
    pub agent_timeout_s: u64,
}

/// Verification gate configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verification {
    /// Shell command to run after each slice.  Exit code 0 means pass.
    pub command: String,
}

/// Per-slice review policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    /// When `true`, a reviewer role runs after every slice.
    pub per_slice: bool,
    /// When `true`, a failing review blocks progression to the next slice.
    pub required: bool,
}

/// Publication constraints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Publication {
    /// Maximum lines changed permitted in a single pull-request slice.
    pub max_pr_lines: u32,
}

impl LoopConfig {
    /// Loads a [`LoopConfig`] from a JSON file at `path`.
    ///
    /// Computes the SHA-256 policy hash of the raw file content and stores
    /// it in [`LoopConfig::policy_hash`] for later tamper detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read or the JSON is invalid.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut cfg: Self = serde_json::from_str(&content)?;
        cfg.policy_hash = Self::hash_policy(&content);
        Ok(cfg)
    }

    /// Computes the SHA-256 hash of `content` and returns it as a lowercase hex string.
    #[must_use]
    pub fn hash_policy(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Verifies that the policy file at `path` still matches the hash computed at load.
    ///
    /// # Errors
    ///
    /// Returns an error with a `PolicyTampered` prefix when the hashes differ,
    /// or any I/O error when the file cannot be read.
    pub fn verify_policy(&self, path: &Path) -> anyhow::Result<()> {
        let current = std::fs::read_to_string(path)?;
        let current_hash = Self::hash_policy(&current);
        if current_hash != self.policy_hash {
            anyhow::bail!(
                "PolicyTampered: loop.json hash mismatch (expected {}, got {})",
                self.policy_hash,
                current_hash
            );
        }
        Ok(())
    }

    /// Returns the reviewer role if present, or `None`.
    #[must_use]
    pub fn reviewer(&self) -> Option<&LoopRole> {
        self.roles.iter().find(|r| r.name == "reviewer")
    }

    /// Returns the implementer role if present, or `None`.
    #[must_use]
    pub fn implementer(&self) -> Option<&LoopRole> {
        self.roles.iter().find(|r| r.name == "implementer")
    }

    /// Returns `true` when reviewer and implementer use different runner backends.
    ///
    /// When both roles are absent the constraint is vacuously satisfied.
    #[must_use]
    pub fn evaluator_separation_satisfied(&self) -> bool {
        match (self.reviewer(), self.implementer()) {
            (Some(rev), Some(imp)) => rev.runner_differs_from(imp),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn minimal_json(version: u32) -> String {
        format!(
            r#"{{
                "version": {version},
                "limits": {{"max_attempts": 3, "agent_timeout_s": 60}},
                "roles": [],
                "verification": {{"command": ".smedja/bin/verify.sh"}},
                "review": {{"per_slice": true, "required": true}},
                "publication": {{"max_pr_lines": 400}}
            }}"#
        )
    }

    #[test]
    fn loop_config_loads_from_json() {
        let json = minimal_json(1);
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("loop.json");
        std::fs::write(&path, &json).unwrap();

        let cfg = LoopConfig::from_file(&path).unwrap();
        assert_eq!(cfg.version, 1);
        assert_eq!(cfg.limits.max_attempts, 3);
        assert_eq!(cfg.limits.agent_timeout_s, 60);
        assert_eq!(cfg.verification.command, ".smedja/bin/verify.sh");
        assert!(cfg.review.per_slice);
        assert!(cfg.review.required);
        assert_eq!(cfg.publication.max_pr_lines, 400);
        assert!(!cfg.policy_hash.is_empty());
    }

    #[test]
    fn policy_hash_is_stable_for_identical_content() {
        let content = r#"{"version":1}"#;
        let h1 = LoopConfig::hash_policy(content);
        let h2 = LoopConfig::hash_policy(content);
        assert_eq!(h1, h2);
    }

    #[test]
    fn policy_hash_differs_for_different_content() {
        let h1 = LoopConfig::hash_policy(r#"{"version":1}"#);
        let h2 = LoopConfig::hash_policy(r#"{"version":2}"#);
        assert_ne!(h1, h2);
    }

    #[test]
    fn policy_tamper_detected() {
        let json = minimal_json(1);
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("loop.json");
        std::fs::write(&path, &json).unwrap();

        let cfg = LoopConfig::from_file(&path).unwrap();

        // Tamper the file on disk.
        std::fs::write(&path, minimal_json(99)).unwrap();

        let result = cfg.verify_policy(&path);
        assert!(result.is_err(), "tamper must be detected");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("PolicyTampered"),
            "error must mention PolicyTampered, got: {msg}"
        );
    }

    #[test]
    fn policy_verify_passes_when_unchanged() {
        let json = minimal_json(1);
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("loop.json");
        std::fs::write(&path, &json).unwrap();

        let cfg = LoopConfig::from_file(&path).unwrap();
        assert!(cfg.verify_policy(&path).is_ok());
    }

    #[test]
    fn evaluator_separation_vacuously_satisfied_with_no_roles() {
        let json = minimal_json(1);
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("loop.json");
        std::fs::write(&path, &json).unwrap();
        let cfg = LoopConfig::from_file(&path).unwrap();
        assert!(cfg.evaluator_separation_satisfied());
    }
}
