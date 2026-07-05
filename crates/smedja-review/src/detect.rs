//! Stage 0 — language detection from changed paths.
//!
//! Review is scoped to changed code ("clean as you code"), so the languages to
//! grade are inferred from the set of touched paths rather than a whole-repo
//! scan. Each path maps to at most one [`Language`] by extension; the returned
//! set is de-duplicated and stably ordered.

use serde::{Deserialize, Serialize};

/// A language whose canonical review tools smedja knows how to drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    /// Rust.
    Rust,
    /// Python.
    Python,
    /// JavaScript / TypeScript (shared toolchain: prettier, eslint).
    JavaScript,
    /// Go.
    Go,
}

impl Language {
    /// The display label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::Go => "go",
        }
    }

    /// Maps a file extension (without the dot) to a language, if recognised.
    #[must_use]
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "rs" => Some(Self::Rust),
            "py" | "pyi" => Some(Self::Python),
            "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Some(Self::JavaScript),
            "go" => Some(Self::Go),
            _ => None,
        }
    }
}

/// Returns the distinct languages touched by `paths`, in a stable order.
#[must_use]
pub fn languages_from_paths<S: AsRef<str>>(paths: &[S]) -> Vec<Language> {
    let mut found: Vec<Language> = Vec::new();
    for p in paths {
        let Some(ext) = std::path::Path::new(p.as_ref())
            .extension()
            .and_then(|e| e.to_str())
        else {
            continue;
        };
        if let Some(lang) = Language::from_extension(ext) {
            if !found.contains(&lang) {
                found.push(lang);
            }
        }
    }
    found
}

/// Filters `paths` to those belonging to `lang` (by extension).
#[must_use]
pub fn paths_for_language<S: AsRef<str>>(paths: &[S], lang: Language) -> Vec<&str> {
    paths
        .iter()
        .map(std::convert::AsRef::as_ref)
        .filter(|p| {
            std::path::Path::new(p)
                .extension()
                .and_then(|e| e.to_str())
                .and_then(Language::from_extension)
                == Some(lang)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_distinct_languages() {
        let paths = [
            "src/main.rs",
            "src/lib.rs",
            "app/api.py",
            "web/ui.tsx",
            "cmd/serve.go",
            "README.md",
        ];
        let langs = languages_from_paths(&paths);
        assert_eq!(
            langs,
            vec![
                Language::Rust,
                Language::Python,
                Language::JavaScript,
                Language::Go
            ]
        );
    }

    #[test]
    fn unknown_extensions_are_ignored() {
        let langs = languages_from_paths(&["notes.txt", "data.csv", "Makefile"]);
        assert!(langs.is_empty());
    }

    #[test]
    fn paths_for_language_filters_by_extension() {
        let paths = ["a.rs", "b.py", "c.rs"];
        let rust = paths_for_language(&paths, Language::Rust);
        assert_eq!(rust, vec!["a.rs", "c.rs"]);
    }
}
