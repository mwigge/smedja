//! Tool-output secret scanning.
//!
//! [`scan_output`] runs a small set of compiled, high-signal secret/credential
//! patterns over a tool-result string. It is advisory by default: a match
//! produces a [`Finding`] but the content is returned **unmodified**. Only when
//! the active [`SecurityConfig`] enforces at or above the match severity is the
//! matched span replaced with a redaction placeholder.
//!
//! Scanning is skipped entirely when the bypass environment variable
//! [`BYPASS_ENV`] is set to a non-empty value, mirroring the existing
//! crusher/verbosity bypasses.

use std::borrow::Cow;
use std::sync::OnceLock;

use regex::Regex;

use crate::config::SecurityConfig;
use crate::finding::{Finding, Severity};

/// Environment variable that, when set to a non-empty value, disables output
/// scanning entirely.
pub const BYPASS_ENV: &str = "SMEDJA_NO_OUTPUT_SCAN";

/// Placeholder substituted for a matched secret when redaction is enforced.
pub const REDACTION_PLACEHOLDER: &str = "[REDACTED]";

/// The outcome of scanning a tool-result string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputScan {
    /// Findings detected in the scanned content.
    pub findings: Vec<Finding>,
    /// The content to return to the caller — identical to the input unless
    /// enforcement triggered redaction.
    pub content: String,
}

impl OutputScan {
    /// Returns `true` when no secret pattern matched.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// A single high-signal secret pattern paired with its severity and rule id.
struct SecretPattern {
    rule_id: &'static str,
    severity: Severity,
    regex: Regex,
}

/// Returns the compiled secret-pattern set, built once.
fn secret_patterns() -> &'static Vec<SecretPattern> {
    static PATTERNS: OnceLock<Vec<SecretPattern>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        let raw: &[(&str, Severity, &str)] = &[
            // AWS access key id.
            ("aws-access-key", Severity::Critical, r"AKIA[0-9A-Z]{16}"),
            // Generic private-key PEM header.
            (
                "private-key",
                Severity::Critical,
                r"-----BEGIN (?:RSA |EC |OPENSSH |DSA )?PRIVATE KEY-----",
            ),
            // GitHub personal access / fine-grained tokens.
            (
                "github-token",
                Severity::High,
                r"gh[pousr]_[0-9A-Za-z]{36,}",
            ),
            // Slack token.
            (
                "slack-token",
                Severity::High,
                r"xox[baprs]-[0-9A-Za-z-]{10,}",
            ),
            // Generic provider secret-key prefix (e.g. sk-... API keys).
            ("api-key", Severity::High, r"sk-[0-9A-Za-z]{20,}"),
        ];
        raw.iter()
            .map(|(rule_id, severity, pat)| SecretPattern {
                rule_id,
                severity: *severity,
                regex: Regex::new(pat).expect("invalid secret pattern regex"),
            })
            .collect()
    })
}

/// Scans `content` for secret patterns under the active `config`.
///
/// Returns an [`OutputScan`] whose `findings` describe every matched pattern.
/// The returned `content` is the original input unless `config` enforces at or
/// above a match's severity, in which case the matched spans for that pattern
/// are replaced with [`REDACTION_PLACEHOLDER`]. When the [`BYPASS_ENV`]
/// environment variable is set, scanning is skipped and the input is returned
/// clean.
#[must_use]
pub fn scan_output(content: &str, config: &SecurityConfig) -> OutputScan {
    scan_output_with_bypass(content, *config, bypass_enabled())
}

/// Core scanner with an explicit `bypass` flag.
///
/// Splitting the bypass decision out of the environment read keeps the matcher
/// deterministic and testable without mutating process-global state.
#[must_use]
fn scan_output_with_bypass(content: &str, config: SecurityConfig, bypass: bool) -> OutputScan {
    if bypass {
        return OutputScan {
            findings: Vec::new(),
            content: content.to_owned(),
        };
    }

    let mut findings = Vec::new();
    let mut current: Cow<'_, str> = Cow::Borrowed(content);

    for pattern in secret_patterns() {
        if !pattern.regex.is_match(&current) {
            continue;
        }
        findings.push(Finding::new(
            pattern.rule_id,
            pattern.severity,
            format!("high-signal secret pattern matched: {}", pattern.rule_id),
        ));
        // Redact only when enforcement is on at or above this severity.
        if config.blocks(pattern.severity) {
            let replaced = pattern
                .regex
                .replace_all(&current, REDACTION_PLACEHOLDER)
                .into_owned();
            current = Cow::Owned(replaced);
        }
    }

    OutputScan {
        findings,
        content: current.into_owned(),
    }
}

/// Returns `true` when the bypass environment variable is set to a non-empty
/// value.
fn bypass_enabled() -> bool {
    std::env::var(BYPASS_ENV).is_ok_and(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Builds a synthetic AWS-style key to exercise the matcher. Assembled from
    // parts so no key-shaped literal appears in source.
    fn sample_secret() -> String {
        format!("AKIA{}", "IOSFODNN7EXAMPLE")
    }

    #[test]
    fn secret_pattern_is_matched_with_severity() {
        let cfg = SecurityConfig::default();
        let scan = scan_output(&format!("k={}", sample_secret()), &cfg);
        assert!(!scan.is_clean());
        assert_eq!(scan.findings[0].rule_id, "aws-access-key");
        assert_eq!(scan.findings[0].severity, Severity::Critical);
    }

    #[test]
    fn clean_string_yields_no_finding() {
        let cfg = SecurityConfig::default();
        let scan = scan_output("hello world, nothing secret here", &cfg);
        assert!(scan.is_clean());
        assert_eq!(scan.content, "hello world, nothing secret here");
    }

    #[test]
    fn advisory_default_returns_content_unmodified() {
        let cfg = SecurityConfig::default();
        let input = format!("k={} done", sample_secret());
        let scan = scan_output(&input, &cfg);
        assert!(!scan.is_clean(), "match must be recorded");
        assert_eq!(scan.content, input, "advisory mode must not redact");
    }

    #[test]
    fn enforcement_at_severity_redacts_match() {
        let cfg = SecurityConfig {
            enforce: true,
            enforce_min_severity: Severity::Critical,
        };
        let secret = sample_secret();
        let input = format!("k={secret} done");
        let scan = scan_output(&input, &cfg);
        assert!(!scan.is_clean());
        assert!(
            scan.content.contains(REDACTION_PLACEHOLDER),
            "enforced redaction must replace the secret: {}",
            scan.content
        );
        assert!(
            !scan.content.contains(&secret),
            "the secret must not survive redaction"
        );
    }

    #[test]
    fn enforcement_below_severity_does_not_redact() {
        // github-token is High; threshold Critical → must not redact.
        let cfg = SecurityConfig {
            enforce: true,
            enforce_min_severity: Severity::Critical,
        };
        let gh_pat = format!("{}_0123456789abcdefghijklmnopqrstuvwxyz", "ghp");
        let scan = scan_output(&format!("t={gh_pat}"), &cfg);
        assert!(!scan.is_clean());
        assert!(
            scan.content.contains(&gh_pat),
            "below-threshold match must not be redacted"
        );
    }

    #[test]
    fn bypass_skips_scanning() {
        // Exercise the bypass branch deterministically via the inner function so
        // the test never mutates process-global env and cannot race concurrent
        // tests in the same binary.
        let cfg = SecurityConfig::default();
        let input = format!("k={}", sample_secret());
        let scan = scan_output_with_bypass(&input, cfg, true);
        assert!(scan.is_clean(), "bypass must skip scanning");
        assert_eq!(scan.content, input);

        // And without bypass the same input is flagged, confirming the bypass
        // flag is what suppressed the finding.
        let scanned = scan_output_with_bypass(&input, cfg, false);
        assert!(!scanned.is_clean());
    }

    #[test]
    fn bypass_enabled_defaults_off_when_env_unset() {
        // With the bypass var absent (the default in the test environment),
        // scanning is active. This avoids mutating the shared environment, which
        // would race concurrent tests in the same binary.
        if std::env::var(BYPASS_ENV).is_err() {
            assert!(
                !bypass_enabled(),
                "bypass must be off when the var is unset"
            );
        }
    }
}
