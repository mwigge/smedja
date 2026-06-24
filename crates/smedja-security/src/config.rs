//! Security-plane configuration resolved from the `[security]` TOML block.
//!
//! The plane is advisory by default: when the `[security]` block is absent the
//! resolved config is `enforce = false`. When the block is present with
//! `enforce = true`, [`SecurityConfig::enforce_min_severity`] defaults to the
//! highest severity so that enabling enforcement still blocks only the most
//! severe findings.

use serde::Deserialize;

use crate::finding::Severity;
use crate::SecurityError;

/// Resolved security-plane configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SecurityConfig {
    /// Whether findings at or above the threshold are promoted to blocks.
    pub enforce: bool,
    /// The minimum severity at which enforcement applies. Defaults to the
    /// highest severity.
    pub enforce_min_severity: Severity,
}

impl Default for SecurityConfig {
    /// The advisory default: enforcement off, threshold at the highest severity.
    fn default() -> Self {
        Self {
            enforce: false,
            enforce_min_severity: Severity::highest(),
        }
    }
}

impl SecurityConfig {
    /// Returns `true` when a finding of `severity` should be blocked under this
    /// config (enforcement on and severity at or above the threshold).
    #[must_use]
    pub fn blocks(&self, severity: Severity) -> bool {
        self.enforce && severity >= self.enforce_min_severity
    }

    /// Resolves a [`SecurityConfig`] from a full TOML document string.
    ///
    /// An absent `[security]` block resolves to the advisory [`Default`]. A
    /// present block with `enforce = true` but no `enforce_min_severity`
    /// defaults the threshold to the highest severity.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::Config`] when the TOML cannot be parsed or
    /// `enforce_min_severity` holds an unrecognised severity string.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, SecurityError> {
        let doc: ConfigDoc =
            toml::from_str(toml_str).map_err(|e| SecurityError::Config(e.to_string()))?;
        let Some(raw) = doc.security else {
            return Ok(Self::default());
        };
        let severity = match raw.enforce_min_severity.as_deref() {
            None => Severity::highest(),
            Some(s) => parse_severity(s)?,
        };
        Ok(Self {
            enforce: raw.enforce.unwrap_or(false),
            enforce_min_severity: severity,
        })
    }
}

/// Parses a lowercase severity string into a [`Severity`].
fn parse_severity(value: &str) -> Result<Severity, SecurityError> {
    match value.to_ascii_lowercase().as_str() {
        "low" => Ok(Severity::Low),
        "medium" => Ok(Severity::Medium),
        "high" => Ok(Severity::High),
        "critical" => Ok(Severity::Critical),
        other => Err(SecurityError::Config(format!(
            "unrecognised enforce_min_severity: {other}"
        ))),
    }
}

/// The relevant slice of a smedja config document.
#[derive(Debug, Deserialize)]
struct ConfigDoc {
    security: Option<RawSecurity>,
}

/// The raw, optional `[security]` block as written in TOML.
#[derive(Debug, Deserialize)]
struct RawSecurity {
    enforce: Option<bool>,
    enforce_min_severity: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_block_resolves_to_advisory_default() {
        let cfg = SecurityConfig::from_toml_str("[other]\nkey = 1\n").unwrap();
        assert!(!cfg.enforce);
        assert_eq!(cfg.enforce_min_severity, Severity::Critical);
        assert_eq!(cfg, SecurityConfig::default());
    }

    #[test]
    fn empty_document_resolves_to_advisory_default() {
        let cfg = SecurityConfig::from_toml_str("").unwrap();
        assert!(!cfg.enforce);
    }

    #[test]
    fn enforce_true_defaults_threshold_to_highest_severity() {
        let cfg = SecurityConfig::from_toml_str("[security]\nenforce = true\n").unwrap();
        assert!(cfg.enforce);
        assert_eq!(cfg.enforce_min_severity, Severity::highest());
    }

    #[test]
    fn explicit_threshold_is_parsed() {
        let cfg = SecurityConfig::from_toml_str(
            "[security]\nenforce = true\nenforce_min_severity = \"high\"\n",
        )
        .unwrap();
        assert!(cfg.enforce);
        assert_eq!(cfg.enforce_min_severity, Severity::High);
    }

    #[test]
    fn unrecognised_severity_is_an_error() {
        let err = SecurityConfig::from_toml_str(
            "[security]\nenforce = true\nenforce_min_severity = \"bogus\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, SecurityError::Config(_)));
    }

    #[test]
    fn blocks_only_at_or_above_threshold_when_enforcing() {
        let cfg = SecurityConfig {
            enforce: true,
            enforce_min_severity: Severity::High,
        };
        assert!(!cfg.blocks(Severity::Medium));
        assert!(cfg.blocks(Severity::High));
        assert!(cfg.blocks(Severity::Critical));
    }

    #[test]
    fn default_config_never_blocks() {
        let cfg = SecurityConfig::default();
        assert!(!cfg.blocks(Severity::Critical));
    }
}
