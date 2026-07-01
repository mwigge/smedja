pub(crate) fn find_gov_artifact(gov_dir: &std::path::Path, id: &str) -> Option<std::path::PathBuf> {
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
