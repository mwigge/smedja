//! `smedja-plugins` — Claude Code skill file manager.
//!
//! Manages `.md` skill files stored under `~/.claude/skills/`. Skills live
//! either as directory-based entries (`<name>/SKILL.md`) or as flat files
//! (`<name>.md`) directly inside the skills directory.

mod error;
mod parse;
mod registry;
mod types;

pub use error::PluginsError;
pub use registry::SkillRegistry;
pub use types::{Skill, SkillManifest};
