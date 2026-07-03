//! `smj gov` — governance artifact management (WIs, RFCs, ADRs).

use std::path::{Path, PathBuf};

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum GovCmd {
    /// List governance artifacts.
    List {
        /// Filter by kind: wi, rfc, adr (default: all).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Transition an artifact to a new status.
    Transition {
        /// Artifact ID (e.g. WI-003).
        id: String,
        /// New status: `planned`, `in_progress`, `done`, `cancelled`.
        status: String,
    },
    /// Create a new work item.
    Create {
        /// Work item title.
        title: String,
        /// Optional description.
        #[arg(long)]
        description: Option<String>,
    },
}

/// Dispatches a `smj gov` subcommand.
pub(crate) fn run(action: GovCmd) -> Result<()> {
    let ws = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match action {
        GovCmd::List { kind } => {
            let gov_dir = ws.join("gov");
            let dirs: Vec<&str> = match kind.as_deref() {
                Some("wi") => vec!["work-items"],
                Some("rfc") => vec!["rfcs"],
                Some("adr") => vec!["adrs"],
                _ => vec!["work-items", "rfcs", "adrs"],
            };
            for dir_name in dirs {
                let dir = gov_dir.join(dir_name);
                if !dir.exists() {
                    continue;
                }
                let mut entries: Vec<_> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
                    .collect();
                entries.sort_by_key(std::fs::DirEntry::file_name);
                for entry in entries {
                    let path = entry.path();
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let id = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let title = text
                        .lines()
                        .find(|l| l.starts_with("title"))
                        .and_then(|l| l.split_once('=').map(|x| x.1))
                        .map(|s| s.trim().trim_matches('"').to_owned())
                        .unwrap_or_default();
                    let status = text
                        .lines()
                        .find(|l| l.starts_with("status"))
                        .and_then(|l| l.split_once('=').map(|x| x.1))
                        .map(|s| s.trim().trim_matches('"').to_owned())
                        .unwrap_or_default();
                    println!("{id:<12}  {status:<14}  {title}");
                }
            }
        }
        GovCmd::Transition { id, status } => {
            const VALID: &[&str] = &["planned", "in_progress", "done", "cancelled"];
            if !VALID.contains(&status.as_str()) {
                eprintln!(
                    "error: invalid status '{status}'. Valid: planned | in_progress | done | cancelled"
                );
                std::process::exit(1);
            }
            let gov_dir = ws.join("gov");
            let id_upper = id.to_uppercase();
            let found = find_gov_artifact(&gov_dir, &id_upper);
            if let Some(path) = found {
                let text = std::fs::read_to_string(&path)?;
                let updated = text
                    .lines()
                    .map(|l| {
                        if l.trim_start().starts_with("status") {
                            format!("status = \"{status}\"")
                        } else {
                            l.to_owned()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::write(&path, updated)?;
                println!("{id_upper}: status \u{2192} {status}");
            } else {
                eprintln!("error: artifact '{id}' not found in gov/");
                std::process::exit(1);
            }
        }
        GovCmd::Create { title, description } => {
            let wi_dir = ws.join("gov").join("work-items");
            std::fs::create_dir_all(&wi_dir)?;
            #[allow(clippy::cast_possible_truncation)]
            let next_n: u32 = std::fs::read_dir(&wi_dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "toml"))
                .count() as u32
                + 1;
            let id = format!("WI-{next_n:03}");
            let desc = description.as_deref().unwrap_or("");
            let toml = format!(
                "id = \"{id}\"\ntitle = \"{title}\"\nstatus = \"planned\"\ndescription = \"{desc}\"\ncreated = \"{}\"\n",
                chrono::Utc::now().format("%Y-%m-%d")
            );
            let path = wi_dir.join(format!("{}.toml", id.to_lowercase()));
            std::fs::write(&path, toml)?;
            println!("Created {id}: {title}");
        }
    }
    Ok(())
}

/// Searches `gov/work-items`, `gov/rfcs`, and `gov/adrs` for a TOML file whose
/// stem matches `id` (case-insensitively, compared in uppercase).
fn find_gov_artifact(gov_dir: &Path, id: &str) -> Option<PathBuf> {
    for subdir in &["work-items", "rfcs", "adrs"] {
        let dir = gov_dir.join(subdir);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let stem = entry
                    .path()
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_uppercase())
                    .unwrap_or_default();
                if stem == id {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}
