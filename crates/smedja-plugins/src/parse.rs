//! YAML frontmatter parser for skill `.md` files.
//!
//! A skill file begins with `---`, followed by YAML content, followed by a
//! second `---` line. Everything after the second `---` is the body.

use std::path::Path;

use serde::Deserialize;

use crate::error::PluginsError;
use crate::types::{Skill, SkillArgument, SkillManifest};

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
    #[serde(default)]
    arguments: Vec<RawArgument>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    supporting_files: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Deserialize)]
struct RawArgument {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    required: bool,
    default: Option<String>,
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

    let arguments = raw
        .metadata
        .arguments
        .into_iter()
        .map(|a| SkillArgument {
            name: a.name,
            description: a.description,
            required: a.required,
            default: a.default,
        })
        .collect();

    Ok(Skill {
        manifest: SkillManifest {
            name: raw.name,
            description: raw.description,
            version: raw.metadata.version,
            trigger_phrases: raw.metadata.trigger_phrases,
            arguments,
            tags: raw.metadata.tags,
            supporting_files: raw.metadata.supporting_files,
            paths: raw.metadata.paths,
        },
        path: path.to_owned(),
        body: body.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// Argument substitution
// ---------------------------------------------------------------------------

/// Substitutes skill argument placeholders in `body` using the provided `args`.
///
/// Placeholder forms:
/// - `$ARGUMENTS` → all args joined by a single space
/// - `$ARGUMENTS[n]` → 0-based positional (empty string if out of bounds)
/// - `$name` → value of the argument named `name`; falls back to its declared
///   `default`, then to an empty string if neither is provided
pub fn apply_skill_arguments(body: &str, args: &[&str], manifest: &SkillManifest) -> String {
    let all = args.join(" ");

    // Replace $ARGUMENTS[n] before $ARGUMENTS to avoid partial matches.
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(pos) = rest.find("$ARGUMENTS") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + "$ARGUMENTS".len()..];
        if let Some(idx_end) = after.strip_prefix('[').and_then(|s| s.find(']')) {
            let idx_str = &after[1..=idx_end];
            let replacement = idx_str
                .parse::<usize>()
                .ok()
                .and_then(|i| args.get(i))
                .copied()
                .unwrap_or("");
            out.push_str(replacement);
            rest = &after[idx_end + 2..]; // skip ']'
        } else {
            out.push_str(&all);
            rest = after;
        }
    }
    out.push_str(rest);

    // Replace $name for each declared argument, longest names first to avoid
    // prefix collisions (e.g. $service before $serv).
    let mut names: Vec<&SkillArgument> = manifest.arguments.iter().collect();
    names.sort_by_key(|a| std::cmp::Reverse(a.name.len()));

    for arg in names {
        let placeholder = format!("${}", arg.name);
        let value = args
            .get(
                manifest
                    .arguments
                    .iter()
                    .position(|a| a.name == arg.name)
                    .unwrap_or(usize::MAX),
            )
            .copied()
            .or(arg.default.as_deref())
            .unwrap_or("");
        out = out.replace(&placeholder, value);
    }

    out
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Splits raw file content into `(yaml_block, body)`.
///
/// Expects the content to start with a line that is exactly `---`, followed
/// by YAML, followed by another line that is exactly `---`. Returns `None`
/// when the structure cannot be found.
pub(crate) fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
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

    use super::{apply_skill_arguments, parse_skill, split_frontmatter};

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

    const WITH_EXTENDED: &str = "\
---
name: deploy
description: Deploy a service.
metadata:
  version: \"2.0.0\"
  trigger_phrases:
    - deploy
  arguments:
    - name: service
      description: Service name to deploy.
      required: true
    - name: env
      description: Target environment.
      required: false
      default: staging
  tags:
    - ops
    - deploy
  supporting_files:
    - helpers/deploy.sh
---
Deploy $service to $env.
$ARGUMENTS[0] is the service.
All args: $ARGUMENTS
";

    #[test]
    fn parse_skill_extracts_arguments() {
        let skill = parse_skill(WITH_EXTENDED, Path::new("/tmp/SKILL.md")).expect("parse");
        let args = &skill.manifest.arguments;
        assert_eq!(args.len(), 2);
        assert_eq!(args[0].name, "service");
        assert!(args[0].required);
        assert!(args[0].default.is_none());
        assert_eq!(args[1].name, "env");
        assert!(!args[1].required);
        assert_eq!(args[1].default.as_deref(), Some("staging"));
    }

    #[test]
    fn parse_skill_extracts_tags_and_supporting_files() {
        let skill = parse_skill(WITH_EXTENDED, Path::new("/tmp/SKILL.md")).expect("parse");
        assert_eq!(skill.manifest.tags, ["ops", "deploy"]);
        assert_eq!(skill.manifest.supporting_files, ["helpers/deploy.sh"]);
    }

    #[test]
    fn apply_arguments_substitutes_named_positional_and_all() {
        let body =
            "Deploy $service to $env.\n$ARGUMENTS[0] is the service.\nAll args: $ARGUMENTS\n";
        let manifest = crate::types::SkillManifest {
            name: "deploy".into(),
            description: String::new(),
            version: None,
            trigger_phrases: vec![],
            arguments: vec![
                crate::types::SkillArgument {
                    name: "service".into(),
                    description: String::new(),
                    required: true,
                    default: None,
                },
                crate::types::SkillArgument {
                    name: "env".into(),
                    description: String::new(),
                    required: false,
                    default: Some("staging".into()),
                },
            ],
            tags: vec![],
            supporting_files: vec![],
            paths: vec![],
        };
        let result = apply_skill_arguments(body, &["api", "prod"], &manifest);
        assert_eq!(
            result,
            "Deploy api to prod.\napi is the service.\nAll args: api prod\n"
        );
    }

    #[test]
    fn apply_arguments_uses_defaults_for_missing_args() {
        let body = "env=$env";
        let manifest = crate::types::SkillManifest {
            name: "deploy".into(),
            description: String::new(),
            version: None,
            trigger_phrases: vec![],
            arguments: vec![
                crate::types::SkillArgument {
                    name: "service".into(),
                    description: String::new(),
                    required: true,
                    default: None,
                },
                crate::types::SkillArgument {
                    name: "env".into(),
                    description: String::new(),
                    required: false,
                    default: Some("staging".into()),
                },
            ],
            tags: vec![],
            supporting_files: vec![],
            paths: vec![],
        };
        let result = apply_skill_arguments(body, &["api"], &manifest);
        assert_eq!(result, "env=staging");
    }

    #[test]
    fn parse_skill_has_empty_extended_fields_by_default() {
        let content = "---\nname: minimal\ndescription: A minimal skill.\n---\nbody\n";
        let skill = parse_skill(content, Path::new("/tmp/SKILL.md")).expect("parse");
        assert!(skill.manifest.arguments.is_empty());
        assert!(skill.manifest.tags.is_empty());
        assert!(skill.manifest.supporting_files.is_empty());
    }
}
