//! `OpenSpecStore` — in-process CRUD for `OpenSpec` change artifacts.
//!
//! Provides typed read/write access to the files that make up an `OpenSpec` change
//! (`proposal.md`, `design.md`, `tasks.md`) stored under an `OpenSpec` changes
//! directory, and a [`OpenSpecStore::list_changes`] helper that enumerates all
//! known changes in the `OpenSpec` root.

use std::path::{Path, PathBuf};

/// In-process CRUD handle for the artifacts of a single `OpenSpec` change.
///
/// Each change lives in its own sub-directory under the `OpenSpec` `changes/`
/// root.  This struct wraps that directory and exposes typed read/write
/// accessors for the three standard artifact files.
pub struct OpenSpecStore {
    base: PathBuf,
}

impl OpenSpecStore {
    /// Returns an `OpenSpecStore` rooted at `change_dir`.
    ///
    /// `change_dir` is expected to be the directory for a specific `OpenSpec`
    /// change, e.g. `openspec/changes/my-feature/`.  The directory is NOT
    /// required to exist at construction time — callers may create it before
    /// calling [`write_proposal`](Self::write_proposal) etc.
    #[must_use]
    pub fn open(change_dir: &Path) -> Self {
        Self {
            base: change_dir.to_owned(),
        }
    }

    // ── proposal.md ──────────────────────────────────────────────────────────

    /// Reads `proposal.md` from the change directory.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the file cannot be read.
    pub fn read_proposal(&self) -> std::io::Result<String> {
        std::fs::read_to_string(self.base.join("proposal.md"))
    }

    /// Writes `content` to `proposal.md` in the change directory.
    ///
    /// Creates the directory tree if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the directory cannot be created or the
    /// file cannot be written.
    pub fn write_proposal(&self, content: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::write(self.base.join("proposal.md"), content)
    }

    // ── design.md ────────────────────────────────────────────────────────────

    /// Reads `design.md` from the change directory.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the file cannot be read.
    pub fn read_design(&self) -> std::io::Result<String> {
        std::fs::read_to_string(self.base.join("design.md"))
    }

    /// Writes `content` to `design.md` in the change directory.
    ///
    /// Creates the directory tree if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the directory cannot be created or the
    /// file cannot be written.
    pub fn write_design(&self, content: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::write(self.base.join("design.md"), content)
    }

    // ── tasks.md ─────────────────────────────────────────────────────────────

    /// Reads `tasks.md` from the change directory.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the file cannot be read.
    pub fn read_tasks(&self) -> std::io::Result<String> {
        std::fs::read_to_string(self.base.join("tasks.md"))
    }

    /// Writes `content` to `tasks.md` in the change directory.
    ///
    /// Creates the directory tree if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if the directory cannot be created or the
    /// file cannot be written.
    pub fn write_tasks(&self, content: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::write(self.base.join("tasks.md"), content)
    }

    // ── directory listing ─────────────────────────────────────────────────────

    /// Returns the names of all change directories under `openspec_root`.
    ///
    /// Only immediate subdirectories of `openspec_root` are returned — entries
    /// that are not directories are silently skipped.  The result is sorted
    /// alphabetically.
    ///
    /// # Errors
    ///
    /// Returns [`std::io::Error`] if `openspec_root` cannot be read.
    pub fn list_changes(openspec_root: &Path) -> std::io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in std::fs::read_dir(openspec_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let name = entry.file_name().to_string_lossy().into_owned();
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_proposal_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = OpenSpecStore::open(dir.path());
        store.write_proposal("# My proposal\n").unwrap();
        assert_eq!(store.read_proposal().unwrap(), "# My proposal\n");
    }

    #[test]
    fn write_then_read_design_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = OpenSpecStore::open(dir.path());
        store.write_design("## Design\n").unwrap();
        assert_eq!(store.read_design().unwrap(), "## Design\n");
    }

    #[test]
    fn write_then_read_tasks_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let store = OpenSpecStore::open(dir.path());
        store.write_tasks("- [ ] task one\n").unwrap();
        assert_eq!(store.read_tasks().unwrap(), "- [ ] task one\n");
    }

    #[test]
    fn write_creates_directory_tree() {
        let dir = tempfile::tempdir().unwrap();
        // Point the store at a non-existent sub-directory.
        let nested = dir.path().join("changes").join("my-change");
        let store = OpenSpecStore::open(&nested);
        store.write_proposal("hello").unwrap();
        assert!(nested.join("proposal.md").exists());
    }

    #[test]
    fn list_changes_returns_sorted_directory_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("beta")).unwrap();
        std::fs::create_dir_all(root.path().join("alpha")).unwrap();
        std::fs::create_dir_all(root.path().join("gamma")).unwrap();
        // A plain file should be ignored.
        std::fs::write(root.path().join("not-a-dir.md"), "").unwrap();

        let names = OpenSpecStore::list_changes(root.path()).unwrap();
        assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn list_changes_empty_root_returns_empty_vec() {
        let root = tempfile::tempdir().unwrap();
        let names = OpenSpecStore::list_changes(root.path()).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn overwrite_proposal_replaces_content() {
        let dir = tempfile::tempdir().unwrap();
        let store = OpenSpecStore::open(dir.path());
        store.write_proposal("first").unwrap();
        store.write_proposal("second").unwrap();
        assert_eq!(store.read_proposal().unwrap(), "second");
    }
}
