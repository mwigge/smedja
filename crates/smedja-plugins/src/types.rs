//! Public data types for skill manifests and skill records.

use std::path::PathBuf;

/// Parsed metadata from a skill's YAML frontmatter.
#[derive(Debug, Clone, PartialEq)]
pub struct SkillManifest {
    /// The canonical skill name as declared in frontmatter.
    pub name: String,
    /// Human-readable description of the skill.
    pub description: String,
    /// Optional semantic version string from `metadata.version`.
    pub version: Option<String>,
    /// Optional list of phrases that trigger the skill.
    pub trigger_phrases: Vec<String>,
}

/// A fully loaded skill: its parsed manifest, filesystem path, and body text.
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    /// Parsed YAML frontmatter.
    pub manifest: SkillManifest,
    /// Absolute path to the `SKILL.md` (or flat `.md`) file.
    pub path: PathBuf,
    /// Everything after the closing `---` delimiter in the file.
    pub body: String,
}
