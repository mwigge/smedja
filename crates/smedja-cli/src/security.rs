use super::*;

pub(crate) fn cmd_security_scan(workspace: &std::path::Path) {
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

pub(crate) fn cmd_security_report(ingot: &Ingot) -> Result<()> {
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

pub(crate) fn cmd_security_sbom(lockfile: &std::path::Path) -> Result<()> {
    let sbom = smedja_security::Sbom::from_lockfile(lockfile)
        .with_context(|| format!("failed to assemble SBOM from {}", lockfile.display()))?;
    let json = sbom.to_json().context("failed to serialise SBOM")?;
    println!("{json}");
    Ok(())
}
