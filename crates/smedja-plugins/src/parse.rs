//! YAML frontmatter parser for skill `.md` files.
//!
//! A skill file begins with `---`, followed by YAML content, followed by a
//! second `---` line. Everything after the second `---` is the body.

use std::path::Path;

use serde::Deserialize;

use crate::error::PluginsError;
use crate::types::{Skill, SkillManifest};

// ---------------------------------------------------------------------------
// Raw deserialization types (not part of the public API)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct RawFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    metadata: RawMetadata,
}

#[derive(Default, Deserialize)]
struct RawMetadata {
    version: Option<String>,
    #[serde(default)]
    trigger_phrases: Vec<String>,
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Parses a skill file from its raw text content and filesystem path.
///
/// # Errors
///
/// Returns [`PluginsError::ParseFailed`] when the file does not begin with
/// `---`, contains no closing `---`, or the YAML is malformed.
pub fn parse_skill(content: &str, path: &Path) -> Result<Skill, PluginsError> {
    let (yaml, body) = split_frontmatter(content).ok_or_else(|| PluginsError::ParseFailed {
        path: path.to_owned(),
        reason: "file does not contain valid `---` frontmatter delimiters".into(),
    })?;

    let raw: RawFrontmatter =
        serde_yaml::from_str(yaml).map_err(|e| PluginsError::ParseFailed {
            path: path.to_owned(),
            reason: e.to_string(),
        })?;

    Ok(Skill {
        manifest: SkillManifest {
            name: raw.name,
            description: raw.description,
            version: raw.metadata.version,
            trigger_phrases: raw.metadata.trigger_phrases,
        },
        path: path.to_owned(),
        body: body.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Splits raw file content into `(yaml_block, body)`.
///
/// Expects the content to start with a line that is exactly `---`, followed
/// by YAML, followed by another line that is exactly `---`. Returns `None`
/// when the structure cannot be found.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.trim_start_matches('\u{feff}'); // strip UTF-8 BOM if present

    // The file must begin with the opening delimiter.
    let after_open = content.strip_prefix("---")?;

    // The opening `---` may be followed by a newline (UNIX or Windows style).
    let after_open = after_open
        .strip_prefix('\n')
        .or_else(|| after_open.strip_prefix("\r\n"))?;

    // Find the closing `---` as a line boundary match.
    let close_pattern = "\n---";
    let close_pos = after_open.find(close_pattern)?;

    let yaml = &after_open[..close_pos];
    let rest = &after_open[close_pos + close_pattern.len()..];

    // The body is everything after the closing delimiter line, skipping the
    // immediate newline that terminates the `---` line itself.
    let body = rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
        .unwrap_or(rest);

    Some((yaml, body))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{parse_skill, split_frontmatter};

    const VALID: &str = "\
---
name: rust
description: Comprehensive Rust engineering skill.
metadata:
  version: \"1.0.0\"
  trigger_phrases:
    - rust
    - cargo
---
# Body content here
";

    #[test]
    fn split_roundtrips_yaml_and_body() {
        let (yaml, body) = split_frontmatter(VALID).expect("should split");
        assert!(yaml.contains("name: rust"));
        assert!(body.contains("# Body content here"));
    }

    #[test]
    fn parse_skill_extracts_all_fields() {
        let skill = parse_skill(VALID, Path::new("/tmp/SKILL.md")).expect("should parse");
        assert_eq!(skill.manifest.name, "rust");
        assert_eq!(skill.manifest.version.as_deref(), Some("1.0.0"));
        assert_eq!(skill.manifest.trigger_phrases, ["rust", "cargo"]);
        assert!(skill.body.contains("# Body content here"));
    }

    #[test]
    fn parse_skill_handles_missing_metadata() {
        let content = "---\nname: minimal\ndescription: A minimal skill.\n---\nbody\n";
        let skill = parse_skill(content, Path::new("/tmp/SKILL.md")).expect("should parse");
        assert_eq!(skill.manifest.name, "minimal");
        assert!(skill.manifest.version.is_none());
        assert!(skill.manifest.trigger_phrases.is_empty());
    }

    #[test]
    fn parse_skill_returns_error_on_missing_delimiter() {
        let bad = "name: rust\ndescription: missing delimiters\n";
        assert!(parse_skill(bad, Path::new("/tmp/SKILL.md")).is_err());
    }

    #[test]
    fn parse_skill_returns_error_on_invalid_yaml() {
        let bad = "---\n: broken: yaml: here\n---\nbody\n";
        assert!(parse_skill(bad, Path::new("/tmp/SKILL.md")).is_err());
    }
}
