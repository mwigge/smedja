//! Skill-directory helpers for the slash-command system: locating the user
//! skills dir and listing / installing skill folders. Moved verbatim from
//! `slash.rs`.

use std::path::{Path, PathBuf};

/// Home directory (`$HOME`, falling back to `.`).
pub(crate) fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("."), PathBuf::from)
}

/// Lists skill names in `dir`: `<name>.md` → `name`, `<name>/SKILL.md` → `name`.
pub(crate) fn list_skill_dir(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                if path.join("SKILL.md").exists() {
                    if let Some(n) = path.file_name().and_then(|s| s.to_str()) {
                        out.push(n.to_owned());
                    }
                }
            } else if path.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(n) = path.file_stem().and_then(|s| s.to_str()) {
                    out.push(n.to_owned());
                }
            }
        }
    }
    out.sort();
    out
}

/// Copies every `*.md` from `src` into the workspace skills dir `dst`, creating
/// `dst` as needed. Returns a status string.
pub(crate) fn install_skills_dir(src: &Path, dst: &Path) -> String {
    if !src.is_dir() {
        return format!("skills: {} is not a directory", src.display());
    }
    if std::fs::create_dir_all(dst).is_err() {
        return "skills: cannot create .smedja/skills".to_owned();
    }
    let mut n = 0u32;
    if let Ok(rd) = std::fs::read_dir(src) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("md") {
                if let Some(name) = p.file_name() {
                    if std::fs::copy(&p, dst.join(name)).is_ok() {
                        n += 1;
                    }
                }
            }
        }
    }
    format!(
        "\u{2713} installed {n} skill file(s) into {} — auto-injected next turn",
        dst.display()
    )
}
