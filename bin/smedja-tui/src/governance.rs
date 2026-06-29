/// Formats a list of govctl artifacts for display in the main panel.
pub(crate) fn format_gov_list(artifacts: &[GovArtifact]) -> String {
    if artifacts.is_empty() {
        return "gov: no artifacts found in ./gov/ — create gov/work-items/WI-001.toml to start"
            .to_owned();
    }
    let mut lines = vec![format!("{} govctl artifact(s):", artifacts.len())];
    for a in artifacts {
        lines.push(format!(
            "  [{:12}] {:10} [{:12}] {}",
            a.id, a.kind, a.status, a.title
        ));
    }
    lines.join("\n")
}

/// A single govctl artifact read from a `gov/` TOML file.
#[derive(Debug, Clone)]
pub(crate) struct GovArtifact {
    /// E.g. "WI-001", "RFC-001", "ADR-001"
    pub(crate) id: String,
    /// Artifact kind: "work-item", "rfc", or "adr"
    pub(crate) kind: String,
    pub(crate) title: String,
    /// `"planned"` | `"in_progress"` | `"done"` | `"cancelled"` | `"draft"` | `"accepted"` | `"superseded"`
    pub(crate) status: String,
}

/// Returns the manifest file names present in `workspace`, in detection order:
/// Cargo.toml, package.json, go.mod, pyproject.toml.
pub(crate) fn detect_project_types(workspace: &std::path::Path) -> Vec<&'static str> {
    [
        workspace
            .join("Cargo.toml")
            .exists()
            .then_some("Cargo.toml"),
        workspace
            .join("package.json")
            .exists()
            .then_some("package.json"),
        workspace.join("go.mod").exists().then_some("go.mod"),
        workspace
            .join("pyproject.toml")
            .exists()
            .then_some("pyproject.toml"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Scans `<workspace>/gov/` for TOML files and parses them as govctl artifacts.
///
/// Expected TOML fields: `id`, `title`, `status` (required), `type`/`kind` (optional).
/// Files that fail to parse are silently skipped.
pub(crate) fn scan_gov_artifacts(workspace: &std::path::Path) -> Vec<GovArtifact> {
    let gov_dir = workspace.join("gov");
    if !gov_dir.is_dir() {
        return Vec::new();
    }
    let mut artifacts = Vec::new();
    let subdirs = ["work-items", "rfc", "adr", ""];
    for sub in &subdirs {
        let dir = if sub.is_empty() {
            gov_dir.clone()
        } else {
            gov_dir.join(sub)
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(val) = toml::from_str::<toml::Value>(&raw) else {
                continue;
            };
            let Some(id) = val.get("id").and_then(toml::Value::as_str) else {
                continue;
            };
            let title = val
                .get("title")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_owned();
            let status = val
                .get("status")
                .and_then(toml::Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let kind = if sub.is_empty() {
                val.get("kind")
                    .or_else(|| val.get("type"))
                    .and_then(toml::Value::as_str)
                    .unwrap_or("artifact")
                    .to_owned()
            } else {
                (*sub).to_owned()
            };
            artifacts.push(GovArtifact {
                id: id.to_owned(),
                kind,
                title,
                status,
            });
        }
    }
    artifacts.sort_by(|a, b| a.id.cmp(&b.id));
    artifacts
}

/// Creates a new govctl artifact TOML file in the appropriate subdirectory.
///
/// `rest` is the tail of `/gov create <rest>`, e.g. `work-item My title here`.
/// Auto-increments the numeric suffix (WI-NNN, RFC-NNN, ADR-NNN) by scanning
/// existing files.  Returns a human-readable outcome string.
pub(crate) fn gov_create(workspace: &std::path::Path, rest: &str) -> String {
    let (kind, prefix, subdir, default_status) = if let Some(title) = rest
        .strip_prefix("work-item ")
        .or_else(|| rest.strip_prefix("work-items "))
    {
        (title.trim(), "WI", "work-items", "planned")
    } else if let Some(title) = rest.strip_prefix("rfc ") {
        (title.trim(), "RFC", "rfc", "draft")
    } else if let Some(title) = rest.strip_prefix("adr ") {
        (title.trim(), "ADR", "adr", "draft")
    } else {
        return "gov create: unknown kind — try: work-item | rfc | adr".to_owned();
    };

    if kind.is_empty() {
        return "gov create: title is required".to_owned();
    }

    let dir = workspace.join("gov").join(subdir);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("gov create: could not create {}: {e}", dir.display());
    }

    #[allow(clippy::maybe_infinite_iter)]
    let next_n = (1u32..)
        .find(|n| !dir.join(format!("{prefix}-{n:03}.toml")).exists())
        .unwrap_or(1);
    let id = format!("{prefix}-{next_n:03}");
    let path = dir.join(format!("{id}.toml"));

    let content =
        format!("id     = \"{id}\"\ntitle  = \"{kind}\"\nstatus = \"{default_status}\"\n");
    match std::fs::write(&path, content) {
        Ok(()) => format!("gov: created {id} — {kind}"),
        Err(e) => format!("gov create: write failed: {e}"),
    }
}

/// Transitions a govctl artifact to a new status.
///
/// `rest` is `<id> <new-status>`.  Reads the existing TOML file, replaces the
/// `status` line, and writes it back.  Returns a human-readable outcome string.
pub(crate) fn gov_transition(workspace: &std::path::Path, rest: &str) -> String {
    let valid_wi = ["planned", "in_progress", "done", "cancelled"];
    let valid_rfc_adr = ["draft", "accepted", "rejected", "superseded"];

    let mut parts = rest.splitn(2, ' ');
    let id = match parts.next() {
        Some(s) if !s.is_empty() => s,
        _ => return "gov transition: usage: /gov transition <id> <status>".to_owned(),
    };
    let new_status = match parts.next() {
        Some(s) if !s.is_empty() => s.trim(),
        _ => return "gov transition: status is required".to_owned(),
    };

    let prefix = id.split('-').next().unwrap_or("");
    let valid = if prefix == "WI" {
        &valid_wi[..]
    } else {
        &valid_rfc_adr[..]
    };
    if !valid.contains(&new_status) {
        return format!(
            "gov transition: invalid status '{new_status}' for {prefix} — valid: {}",
            valid.join(", ")
        );
    }

    let subdirs = ["work-items", "rfc", "adr", ""];
    let path = subdirs.iter().find_map(|sub| {
        let dir = if sub.is_empty() {
            workspace.join("gov")
        } else {
            workspace.join("gov").join(sub)
        };
        let p = dir.join(format!("{id}.toml"));
        if p.exists() {
            Some(p)
        } else {
            None
        }
    });

    let Some(path) = path else {
        return format!("gov transition: artifact '{id}' not found");
    };

    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => return format!("gov transition: read failed: {e}"),
    };

    let updated: String = raw
        .lines()
        .map(|line| {
            if line.trim_start().starts_with("status") && line.contains('=') {
                format!("status = \"{new_status}\"")
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let updated = if raw.ends_with('\n') {
        format!("{updated}\n")
    } else {
        updated
    };

    match std::fs::write(&path, updated) {
        Ok(()) => format!("gov: {id} → {new_status}"),
        Err(e) => format!("gov transition: write failed: {e}"),
    }
}
