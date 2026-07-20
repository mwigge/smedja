//! `smedja-plugins` — skill file manager.
//!
//! Manages `.md` skill files stored under `~/.config/smedja/skills/` (with a
//! legacy fallback to `~/.claude/skills/`). Skills live either as directory-
//! based entries (`<name>/SKILL.md`) or as flat files (`<name>.md`) directly
//! inside the skills directory.

mod bundle;
mod error;
mod parse;
mod registry;
mod selector;
mod types;

pub use bundle::{parse_agent_item, parse_skill_item, AgentDef, Bundle, BundleItem, BundleKind};
pub use error::PluginsError;
pub use parse::{apply_skill_arguments, parse_skill};
pub use registry::{SkillRegistry, SyncResult};
pub use selector::{glob_match, select};
pub use types::{Skill, SkillArgument, SkillManifest};

/// Wraps a skill body in an XML envelope, escaping `<`, `>`, and `&` in the
/// body to prevent envelope breakout.
///
/// Produces:
/// ```xml
/// <skill_content name="name">
/// escaped body
/// </skill_content>
/// ```
#[must_use]
pub fn wrap_skill_body(name: &str, body: &str) -> String {
    let escaped_name = name
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let escaped_body = body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!("<skill_content name=\"{escaped_name}\">\n{escaped_body}\n</skill_content>")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_skill_body_escapes_script_tag() {
        let out = wrap_skill_body("test", "<script>alert(1)</script>");
        assert!(!out.contains("<script>"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn wrap_skill_body_escapes_name_ampersand() {
        let out = wrap_skill_body("foo&bar", "body");
        assert!(out.contains("foo&amp;bar"));
    }

    #[test]
    fn wrap_skill_body_round_trip_extractable() {
        let out = wrap_skill_body("my-skill", "some content");
        assert!(out.starts_with("<skill_content name=\"my-skill\">"));
        assert!(out.ends_with("</skill_content>"));
        // body is extractable between the tags
        assert!(out.contains("some content"));
    }
}
