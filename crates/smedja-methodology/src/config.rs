//! Foundational-discipline configuration resolved from the `[methodology]` TOML
//! block.
//!
//! The discipline is on by default: when the `[methodology]` block (or a field
//! within it) is absent, the resolved config has `tdd = true` and `clean =
//! true`. A workspace opts out of a discipline by setting its flag to `false`,
//! mirroring the per-workspace escape that the security plane offers. The parser
//! is modelled on `smedja_security::SecurityConfig::from_toml_str`.

use serde::Deserialize;

/// An error resolving a [`MethodologyConfig`] from TOML.
#[derive(Debug, thiserror::Error)]
pub enum MethodologyConfigError {
    /// The TOML document could not be parsed.
    #[error("invalid [methodology] config: {0}")]
    Parse(String),
}

/// Resolved foundational-discipline configuration.
///
/// Both disciplines are on by default. Setting a flag to `false` suppresses both
/// the steering directive clause and the diff backstop for that discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodologyConfig {
    /// Whether the TDD discipline (steering + advisory backstop) is active.
    pub tdd: bool,
    /// Whether the clean-code discipline (steering + hard backstop) is active.
    pub clean: bool,
}

impl Default for MethodologyConfig {
    /// The foundational default: both disciplines on.
    fn default() -> Self {
        Self {
            tdd: true,
            clean: true,
        }
    }
}

impl MethodologyConfig {
    /// Resolves a [`MethodologyConfig`] from a full TOML document string.
    ///
    /// An absent `[methodology]` block resolves to the all-on [`Default`]. Within
    /// a present block, each absent field also defaults to `true`.
    ///
    /// # Errors
    ///
    /// Returns [`MethodologyConfigError::Parse`] when the TOML cannot be parsed.
    pub fn from_toml_str(toml_str: &str) -> Result<Self, MethodologyConfigError> {
        let doc: ConfigDoc =
            toml::from_str(toml_str).map_err(|e| MethodologyConfigError::Parse(e.to_string()))?;
        let Some(raw) = doc.methodology else {
            return Ok(Self::default());
        };
        Ok(Self {
            tdd: raw.tdd.unwrap_or(true),
            clean: raw.clean.unwrap_or(true),
        })
    }
}

/// The relevant slice of a smedja config document.
#[derive(Debug, Deserialize)]
struct ConfigDoc {
    methodology: Option<RawMethodology>,
}

/// The raw, optional `[methodology]` block as written in TOML.
#[derive(Debug, Deserialize)]
struct RawMethodology {
    tdd: Option<bool>,
    clean: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_block_resolves_to_all_on_default() {
        let cfg = MethodologyConfig::from_toml_str("[other]\nkey = 1\n").unwrap();
        assert!(cfg.tdd);
        assert!(cfg.clean);
        assert_eq!(cfg, MethodologyConfig::default());
    }

    #[test]
    fn empty_document_resolves_to_all_on_default() {
        let cfg = MethodologyConfig::from_toml_str("").unwrap();
        assert!(cfg.tdd);
        assert!(cfg.clean);
    }

    #[test]
    fn tdd_false_disables_tdd_only() {
        let cfg = MethodologyConfig::from_toml_str("[methodology]\ntdd = false\n").unwrap();
        assert!(!cfg.tdd);
        assert!(cfg.clean);
    }

    #[test]
    fn clean_false_disables_clean_only() {
        let cfg = MethodologyConfig::from_toml_str("[methodology]\nclean = false\n").unwrap();
        assert!(cfg.tdd);
        assert!(!cfg.clean);
    }

    #[test]
    fn both_false_disables_both() {
        let cfg = MethodologyConfig::from_toml_str("[methodology]\ntdd = false\nclean = false\n")
            .unwrap();
        assert!(!cfg.tdd);
        assert!(!cfg.clean);
    }

    #[test]
    fn unparseable_toml_is_an_error() {
        let err = MethodologyConfig::from_toml_str("[methodology\ntdd = ").unwrap_err();
        assert!(matches!(err, MethodologyConfigError::Parse(_)));
    }
}
