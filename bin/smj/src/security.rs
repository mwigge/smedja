//! `smj security` — posture scan, findings report, and SBOM (advisory by default).

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use clap::Subcommand;
use smedja_ingot::Ingot;

use crate::util::default_ingot_path;

#[derive(Subcommand)]
pub(crate) enum SecurityCmd {
    /// Run a workspace posture scan and print the advisory findings
    Scan {
        /// Workspace directory to scan (defaults to the current directory)
        path: Option<PathBuf>,
    },
    /// Summarise recorded `security_finding` audit events (read-only query)
    Report,
    /// Emit a CycloneDX-style SBOM from the resolved Cargo.lock to stdout
    Sbom {
        /// Path to the Cargo.lock to read (defaults to ./Cargo.lock)
        #[arg(long)]
        lockfile: Option<PathBuf>,
    },
}

/// Dispatches a `smj security` subcommand.
pub(crate) fn run(action: SecurityCmd) -> Result<()> {
    match action {
        SecurityCmd::Scan { path } => {
            let target = path.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
            cmd_security_scan(&target);
        }
        SecurityCmd::Report => {
            let db_path = default_ingot_path();
            let ingot = Ingot::open(&db_path)
                .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
            cmd_security_report(&ingot)?;
        }
        SecurityCmd::Sbom { lockfile } => {
            let lock = lockfile.unwrap_or_else(|| PathBuf::from("Cargo.lock"));
            cmd_security_sbom(&lock)?;
        }
    }
    Ok(())
}

/// Runs a workspace posture scan and prints the advisory findings.
///
/// The scan is advisory: it reports findings but blocks nothing. A clean
/// workspace prints a short confirmation.
fn cmd_security_scan(workspace: &Path) {
    let findings = smedja_security::scan_posture(workspace);
    if findings.is_empty() {
        println!("No posture findings in {}", workspace.display());
        return;
    }
    println!("{:<14} {:<10} MESSAGE", "RULE", "SEVERITY");
    println!("{}", "-".repeat(72));
    for f in &findings {
        println!(
            "{:<14} {:<10} {}",
            f.rule_id,
            f.severity.as_str(),
            f.message
        );
    }
    println!(
        "\n{} advisory finding(s) — these do not block any action",
        findings.len()
    );
}

/// Summarises recorded `security_finding` audit events as a read-only query.
fn cmd_security_report(ingot: &Ingot) -> Result<()> {
    let events = ingot
        .list_all_audit_events()
        .context("list_all_audit_events failed")?;
    let findings: Vec<_> = events
        .iter()
        .filter(|e| e.action_type == "security_finding")
        .collect();
    if findings.is_empty() {
        println!("No security findings recorded.");
        return Ok(());
    }
    println!("{:<14} {:<10} {:<8} SESSION", "RULE", "SEVERITY", "STATUS");
    println!("{}", "-".repeat(72));
    for e in &findings {
        println!(
            "{:<14} {:<10} {:<8} {}",
            e.tool_name.as_deref().unwrap_or("-"),
            e.error_kind.as_deref().unwrap_or("-"),
            e.status.as_deref().unwrap_or("-"),
            e.session_id,
        );
    }
    println!("\n{} recorded finding(s)", findings.len());
    Ok(())
}

/// Emits a CycloneDX-style SBOM for `lockfile` to stdout.
fn cmd_security_sbom(lockfile: &Path) -> Result<()> {
    let sbom = smedja_security::Sbom::from_lockfile(lockfile)
        .with_context(|| format!("failed to assemble SBOM from {}", lockfile.display()))?;
    let json = sbom.to_json().context("failed to serialise SBOM")?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_sbom_writes_cyclonedx_document() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("Cargo.lock");
        std::fs::write(
            &lock,
            "version = 3\n\n[[package]]\nname = \"demo\"\nversion = \"1.0.0\"\n",
        )
        .unwrap();
        // Exercise the same path the CLI handler uses.
        let sbom = smedja_security::Sbom::from_lockfile(&lock).unwrap();
        let json = sbom.to_json().unwrap();
        assert!(json.contains("\"bomFormat\": \"CycloneDX\""));
        assert!(json.contains("pkg:cargo/demo@1.0.0"));
        // The handler itself returns Ok on a valid lockfile.
        cmd_security_sbom(&lock).unwrap();
    }

    #[test]
    fn security_report_runs_as_read_only_query() {
        let ingot = smedja_ingot::Ingot::open_in_memory().unwrap();
        // No findings recorded → the read-only query still succeeds.
        cmd_security_report(&ingot).unwrap();
    }

    #[test]
    fn security_scan_reports_findings_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".npmrc"), "//r/:_authToken=x").unwrap();
        // Scan runs regardless of findings (advisory; never blocks).
        cmd_security_scan(dir.path());
        let findings = smedja_security::scan_posture(dir.path());
        assert!(findings.iter().any(|f| f.rule_id == "flagged-path"));
    }
}
