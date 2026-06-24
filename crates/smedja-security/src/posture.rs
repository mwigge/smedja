//! Workspace posture scanning.
//!
//! Walks the workspace root looking for flagged config/hook paths and known
//! IOC file markers, and classifies any candidate shell command through the
//! existing [`smedja_ingot::classify_command`] guard rather than maintaining a
//! second blocklist. Every detection is returned as an advisory [`Finding`];
//! the caller decides whether to enforce. Scan I/O errors are non-fatal: the
//! walker logs a warning and returns the findings gathered so far.

use std::path::Path;

use smedja_ingot::{classify_command, CommandRisk};
use walkdir::WalkDir;

use crate::finding::{Finding, Severity};

/// Maximum directory depth walked during a posture scan, keeping the scan
/// bounded so it never slows startup unduly on deep trees.
const MAX_DEPTH: usize = 6;

/// File names that, when present in a workspace, are flagged as risky
/// configuration or hook surfaces worth an operator's attention.
const FLAGGED_PATHS: &[&str] = &[
    ".npmrc",
    ".pypirc",
    ".netrc",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
];

/// Substrings in a file name that mark a known indicator-of-compromise (IOC)
/// artefact.
const IOC_MARKERS: &[&str] = &["malware", "backdoor", "reverse_shell", "cryptominer"];

/// Scans `workspace_root` for posture findings.
///
/// Returns advisory findings for flagged paths and IOC markers discovered under
/// the root. A non-fatal I/O error while walking logs a warning and yields the
/// partial set gathered before the error rather than failing the whole scan.
#[must_use]
pub fn scan_posture(workspace_root: &Path) -> Vec<Finding> {
    let mut findings = Vec::new();

    for entry in WalkDir::new(workspace_root).max_depth(MAX_DEPTH) {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                // Non-fatal: skip the unreadable path and continue with the
                // findings gathered so far.
                tracing::warn!(error = %err, "posture scan: skipping unreadable path");
                continue;
            }
        };

        let Some(name) = entry.file_name().to_str() else {
            continue;
        };

        if FLAGGED_PATHS.contains(&name) {
            findings.push(Finding::new(
                "flagged-path",
                Severity::Medium,
                format!("flagged credential/config file present: {name}"),
            ));
        }

        let lower = name.to_ascii_lowercase();
        if let Some(marker) = IOC_MARKERS.iter().find(|m| lower.contains(**m)) {
            findings.push(Finding::new(
                "ioc-marker",
                Severity::High,
                format!("indicator-of-compromise marker in file name: {name} ({marker})"),
            ));
        }
    }

    findings
}

/// Classifies a candidate shell `command` into a posture finding by delegating
/// to [`smedja_ingot::classify_command`].
///
/// Returns a highest-severity finding for a [`CommandRisk::Blocked`] command, a
/// medium-severity finding for [`CommandRisk::Confirm`], and `None` for a safe
/// command. This deliberately reuses the existing classifier rather than
/// defining a second blocklist.
#[must_use]
pub fn classify_command_risk(command: &str) -> Option<Finding> {
    match classify_command(command) {
        CommandRisk::Blocked => Some(Finding::new(
            "command-risk",
            Severity::Critical,
            format!("destructive command classified as blocked: {command}"),
        )),
        CommandRisk::Confirm => Some(Finding::new(
            "command-risk",
            Severity::Medium,
            format!("command classified as requiring confirmation: {command}"),
        )),
        CommandRisk::Safe => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flagged_path_yields_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".npmrc"), "//registry/:_authToken=abc").unwrap();

        let findings = scan_posture(dir.path());
        assert!(
            findings.iter().any(|f| f.rule_id == "flagged-path"),
            "flagged path must produce a finding: {findings:?}"
        );
    }

    #[test]
    fn ioc_marker_yields_a_finding() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("reverse_shell.sh"), "#!/bin/sh\n").unwrap();

        let findings = scan_posture(dir.path());
        assert!(
            findings.iter().any(|f| f.rule_id == "ioc-marker"),
            "IOC marker must produce a finding: {findings:?}"
        );
    }

    #[test]
    fn clean_workspace_yields_no_findings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "fn main() {}\n").unwrap();

        let findings = scan_posture(dir.path());
        assert!(
            findings.is_empty(),
            "clean workspace must yield no findings: {findings:?}"
        );
    }

    #[test]
    fn nonexistent_path_is_non_fatal_and_returns_partial() {
        // A missing root produces a walk error on the first entry; the scan must
        // return (an empty vec) rather than panic or error.
        let findings = scan_posture(Path::new("/no/such/path/for/smedja/security/test"));
        assert!(findings.is_empty());
    }

    #[test]
    fn blocked_command_is_highest_severity_finding() {
        let finding = classify_command_risk("rm -rf /").expect("blocked command must be a finding");
        assert_eq!(finding.rule_id, "command-risk");
        assert_eq!(finding.severity, Severity::Critical);
        assert_eq!(finding.severity, Severity::highest());
    }

    #[test]
    fn confirm_command_is_medium_finding() {
        let finding =
            classify_command_risk("git reset --hard").expect("confirm command must be a finding");
        assert_eq!(finding.severity, Severity::Medium);
    }

    #[test]
    fn safe_command_yields_no_finding() {
        assert!(classify_command_risk("ls -la").is_none());
    }
}
