//! One normalized bundle model for runner-agnostic delivery of skills, rules,
//! and subagent definitions.
//!
//! A *bundle* is a single folder tree that feeds every runner. It collapses the
//! two historically parallel skill systems — the raw `.smedja/skills/*.md`
//! concatenation and the [`crate::SkillRegistry`] `SKILL.md` frontmatter parser —
//! into one [`BundleItem`] stream. Three item kinds are recognised:
//!
//! - **Skill** — `skills/<name>/SKILL.md` or a flat `skills/<name>.md`.
//! - **Rule** — `rules/*.md` (always-on advisories; frontmatter optional).
//! - **Agent** — `agents/<name>.md` (subagent definition with `tools`/`model`/
//!   `permissionMode` frontmatter).
//!
//! Every source is routed through the same lenient frontmatter parser: a file
//! without `---` frontmatter still becomes a [`BundleItem`] whose name is the
//! file stem and whose description is derived from the body's first heading or
//! line. This keeps the existing frontmatter-less `.smedja/skills/*.md` files
//! (which the memory loader used to concatenate raw) working unchanged.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::parse::split_frontmatter;

/// The category of a [`BundleItem`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleKind {
    /// A skill: on-demand capability selected by trigger/description/path.
    Skill,
    /// A rule: always-on advisory discipline.
    Rule,
    /// A subagent definition (routing target + tool/model policy).
    Agent,
}

impl BundleKind {
    /// Lowercase identifier used in index lines and MCP surfaces.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            BundleKind::Skill => "skill",
            BundleKind::Rule => "rule",
            BundleKind::Agent => "agent",
        }
    }
}

/// Subagent policy parsed from an `agents/<name>.md` frontmatter block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentDef {
    /// Tool allow-list. Empty means all tools are permitted.
    pub tools: Vec<String>,
    /// Optional model override (e.g. `"claude-sonnet-4-6"`). `None` uses the
    /// runner default.
    pub model: Option<String>,
    /// Optional permission mode (e.g. `"read-only"`, `"acceptEdits"`).
    pub permission_mode: Option<String>,
}

/// A single normalized bundle entry — the one model every runner consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleItem {
    /// Which of the three source layers this item came from.
    pub kind: BundleKind,
    /// Canonical name (frontmatter `name`, else file stem).
    pub name: String,
    /// Human-readable description (frontmatter `description`, else first
    /// heading/line of the body).
    pub description: String,
    /// Phrases that activate the item (frontmatter `metadata.trigger_phrases`).
    pub triggers: Vec<String>,
    /// Glob patterns describing files the item is relevant to
    /// (frontmatter `metadata.paths`).
    pub paths: Vec<String>,
    /// Absolute path to the source `.md` file.
    pub path: PathBuf,
    /// Body text (everything after the closing `---`, or the whole file when
    /// there is no frontmatter).
    pub body: String,
    /// Supporting files declared in frontmatter, relative to the item directory.
    pub supporting_files: Vec<String>,
    /// Subagent policy — present only when `kind == BundleKind::Agent`.
    pub agent: Option<AgentDef>,
}

impl BundleItem {
    /// Returns the item's directory (the parent of its source file).
    #[must_use]
    pub fn dir(&self) -> &Path {
        self.path.parent().unwrap_or_else(|| Path::new(""))
    }

    /// Resolves the absolute paths of this item's supporting files.
    #[must_use]
    pub fn supporting_file_paths(&self) -> Vec<PathBuf> {
        let dir = self.dir();
        self.supporting_files
            .iter()
            .map(|rel| dir.join(rel))
            .collect()
    }

    /// One-line L1 index entry: `- name — description` (description truncated to
    /// its first line). Cheap enough to inject for every item at turn start.
    #[must_use]
    pub fn l1_index_line(&self) -> String {
        let desc = self.description.lines().next().unwrap_or("").trim();
        if desc.is_empty() {
            format!("- {}", self.name)
        } else {
            format!("- {} — {desc}", self.name)
        }
    }
}

// ---------------------------------------------------------------------------
// Raw frontmatter shapes
// ---------------------------------------------------------------------------

/// Skill/rule frontmatter (`name`/`description` top-level, extras under
/// `metadata`). Every field is optional so a partial or absent block still
/// parses.
#[derive(Default, Deserialize)]
struct RawSkillFront {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    metadata: RawSkillMeta,
}

#[derive(Default, Deserialize)]
struct RawSkillMeta {
    #[serde(default)]
    trigger_phrases: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    supporting_files: Vec<String>,
}

/// Agent frontmatter (`.claude/agents/<name>.md` convention): flat top-level
/// keys. `tools` accepts either a YAML list or a comma-separated string.
#[derive(Default, Deserialize)]
struct RawAgentFront {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    tools: ToolsField,
    model: Option<String>,
    #[serde(default, alias = "permission-mode", alias = "permissionMode")]
    permission_mode: Option<String>,
    #[serde(default)]
    trigger_phrases: Vec<String>,
    #[serde(default)]
    paths: Vec<String>,
}

/// A `tools` value that may be written as a YAML sequence or a single
/// comma/space-separated string.
#[derive(Default)]
struct ToolsField(Vec<String>);

impl<'de> Deserialize<'de> for ToolsField {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            List(Vec<String>),
            One(String),
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::List(v) => ToolsField(v),
            Raw::One(s) => ToolsField(
                s.split([',', ' '])
                    .map(str::trim)
                    .filter(|t| !t.is_empty())
                    .map(ToString::to_string)
                    .collect(),
            ),
        })
    }
}

// ---------------------------------------------------------------------------
// Lenient parsers (never fail — a malformed file degrades to file-stem naming)
// ---------------------------------------------------------------------------

/// Derives a description from a body when frontmatter carried none: the first
/// non-empty line, with a leading Markdown heading marker stripped.
fn describe_from_body(body: &str) -> String {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.trim_start_matches('#').trim().to_owned())
        .unwrap_or_default()
}

/// The file stem, used as a fallback name.
fn stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_owned()
}

/// Parses a skill or rule file leniently into a [`BundleItem`].
#[must_use]
pub fn parse_skill_item(content: &str, path: &Path, kind: BundleKind) -> BundleItem {
    let (front, body) = match split_frontmatter(content) {
        Some((yaml, body)) => (
            serde_yaml::from_str::<RawSkillFront>(yaml).unwrap_or_default(),
            body.to_owned(),
        ),
        None => (RawSkillFront::default(), content.to_owned()),
    };
    let name = front.name.unwrap_or_else(|| stem(path));
    let description = front
        .description
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| describe_from_body(&body));
    BundleItem {
        kind,
        name,
        description,
        triggers: front.metadata.trigger_phrases,
        paths: front.metadata.paths,
        path: path.to_owned(),
        body,
        supporting_files: front.metadata.supporting_files,
        agent: None,
    }
}

/// Parses an agent definition file leniently into a [`BundleItem`].
#[must_use]
pub fn parse_agent_item(content: &str, path: &Path) -> BundleItem {
    let (front, body) = match split_frontmatter(content) {
        Some((yaml, body)) => (
            serde_yaml::from_str::<RawAgentFront>(yaml).unwrap_or_default(),
            body.to_owned(),
        ),
        None => (RawAgentFront::default(), content.to_owned()),
    };
    let name = front.name.unwrap_or_else(|| stem(path));
    let description = front
        .description
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| describe_from_body(&body));
    BundleItem {
        kind: BundleKind::Agent,
        name,
        description,
        triggers: front.trigger_phrases,
        paths: front.paths,
        path: path.to_owned(),
        body,
        supporting_files: Vec::new(),
        agent: Some(AgentDef {
            tools: front.tools.0,
            model: front.model,
            permission_mode: front.permission_mode,
        }),
    }
}

// ---------------------------------------------------------------------------
// Bundle
// ---------------------------------------------------------------------------

/// A loaded bundle: every skill, rule, and agent from one or more roots, in one
/// normalized model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bundle {
    /// All parsed items, in deterministic (kind, then name) order.
    pub items: Vec<BundleItem>,
}

impl Bundle {
    /// Loads a bundle from the workspace's `.smedja/` sources and, optionally,
    /// an external bundle root laid out as
    /// `skills/<name>/SKILL.md` + `rules/*.md` + `agents/<name>.md`.
    ///
    /// Both roots are always merged; the external root's items are appended so a
    /// shared toolkit can extend a workspace's local bundle. Unreadable or
    /// malformed files are skipped, never fatal.
    #[must_use]
    pub fn load(workspace_root: &Path, external_root: Option<&Path>) -> Self {
        let mut items = Vec::new();
        Self::load_root(&workspace_root.join(".smedja"), &mut items);
        if let Some(ext) = external_root {
            Self::load_root(ext, &mut items);
        }
        // Deterministic ordering: kind bucket, then name.
        items.sort_by(|a, b| {
            (a.kind.label(), a.name.as_str()).cmp(&(b.kind.label(), b.name.as_str()))
        });
        Self { items }
    }

    /// Loads the `skills/`, `rules/`, and `agents/` subtrees of one root into
    /// `items`.
    fn load_root(root: &Path, items: &mut Vec<BundleItem>) {
        Self::load_skills_dir(&root.join("skills"), items);
        Self::load_flat_dir(&root.join("rules"), BundleKind::Rule, items);
        Self::load_agents_dir(&root.join("agents"), items);
    }

    /// Loads `skills/`: each `<name>/SKILL.md` directory entry and each flat
    /// `<name>.md` file.
    fn load_skills_dir(dir: &Path, items: &mut Vec<BundleItem>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let skill_md = path.join("SKILL.md");
                if let Some(content) = read(&skill_md) {
                    items.push(parse_skill_item(&content, &skill_md, BundleKind::Skill));
                }
            } else if is_md(&path) {
                if let Some(content) = read(&path) {
                    items.push(parse_skill_item(&content, &path, BundleKind::Skill));
                }
            }
        }
    }

    /// Loads a flat directory of `*.md` files as `kind` items.
    fn load_flat_dir(dir: &Path, kind: BundleKind, items: &mut Vec<BundleItem>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_md(&path) {
                if let Some(content) = read(&path) {
                    items.push(parse_skill_item(&content, &path, kind));
                }
            }
        }
    }

    /// Loads `agents/`: each flat `<name>.md` file as an [`BundleKind::Agent`].
    fn load_agents_dir(dir: &Path, items: &mut Vec<BundleItem>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if is_md(&path) {
                if let Some(content) = read(&path) {
                    items.push(parse_agent_item(&content, &path));
                }
            }
        }
    }

    /// Iterates the items of a given kind.
    pub fn of_kind(&self, kind: BundleKind) -> impl Iterator<Item = &BundleItem> {
        self.items.iter().filter(move |i| i.kind == kind)
    }

    /// Iterates skills.
    pub fn skills(&self) -> impl Iterator<Item = &BundleItem> {
        self.of_kind(BundleKind::Skill)
    }

    /// Iterates rules.
    pub fn rules(&self) -> impl Iterator<Item = &BundleItem> {
        self.of_kind(BundleKind::Rule)
    }

    /// Iterates agent definitions.
    pub fn agents(&self) -> impl Iterator<Item = &BundleItem> {
        self.of_kind(BundleKind::Agent)
    }

    /// Finds an item by exact (case-insensitive) name.
    #[must_use]
    pub fn find(&self, name: &str) -> Option<&BundleItem> {
        let n = name.to_lowercase();
        self.items.iter().find(|i| i.name.to_lowercase() == n)
    }

    /// Builds the L1 index — one line per skill and rule (agents are routing
    /// targets, not model-selectable, so they are omitted). Returns `None` when
    /// there is nothing to index.
    #[must_use]
    pub fn l1_index(&self) -> Option<String> {
        let mut lines: Vec<String> = self
            .items
            .iter()
            .filter(|i| i.kind != BundleKind::Agent)
            .map(BundleItem::l1_index_line)
            .collect();
        if lines.is_empty() {
            return None;
        }
        lines.sort();
        Some(lines.join("\n"))
    }
}

/// Reads a file, logging (not failing) on error.
fn read(path: &Path) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "could not read bundle file");
            None
        }
    }
}

/// Returns `true` when `path` is a regular file with a `.md` extension.
fn is_md(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    const SKILL_MD: &str = "\
---
name: postgres-patterns
description: Parameterised query patterns for Postgres.
metadata:
  trigger_phrases:
    - postgres
    - sql
  paths:
    - \"**/*.sql\"
  supporting_files:
    - helpers/schema.sql
---
Use $1 placeholders, never string interpolation.
";

    const AGENT_MD: &str = "\
---
name: reviewer
description: Reviews diffs for correctness.
tools: read_file, grep_files, graph_query
model: claude-sonnet-4-6
permissionMode: read-only
---
You are a careful reviewer.
";

    #[test]
    fn parse_skill_item_reads_frontmatter() {
        let item = parse_skill_item(SKILL_MD, Path::new("/x/SKILL.md"), BundleKind::Skill);
        assert_eq!(item.kind, BundleKind::Skill);
        assert_eq!(item.name, "postgres-patterns");
        assert_eq!(item.triggers, ["postgres", "sql"]);
        assert_eq!(item.paths, ["**/*.sql"]);
        assert_eq!(item.supporting_files, ["helpers/schema.sql"]);
        assert!(item.body.contains("$1 placeholders"));
        assert!(item.agent.is_none());
    }

    #[test]
    fn parse_skill_item_without_frontmatter_uses_stem_and_body() {
        let raw = "# Ponytail\n\nDelete over addition.\n";
        let item = parse_skill_item(raw, Path::new("/x/ponytail.md"), BundleKind::Skill);
        assert_eq!(item.name, "ponytail");
        assert_eq!(item.description, "Ponytail");
        assert!(item.body.contains("Delete over addition."));
        assert!(item.triggers.is_empty());
    }

    #[test]
    fn parse_agent_item_reads_tools_model_and_mode() {
        let item = parse_agent_item(AGENT_MD, Path::new("/x/reviewer.md"));
        assert_eq!(item.kind, BundleKind::Agent);
        assert_eq!(item.name, "reviewer");
        let agent = item.agent.expect("agent def present");
        assert_eq!(agent.tools, ["read_file", "grep_files", "graph_query"]);
        assert_eq!(agent.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(agent.permission_mode.as_deref(), Some("read-only"));
    }

    #[test]
    fn parse_agent_item_accepts_yaml_list_tools() {
        let md = "---\nname: r\ndescription: d\ntools:\n  - read_file\n  - bash\n---\nbody\n";
        let item = parse_agent_item(md, Path::new("/x/r.md"));
        assert_eq!(item.agent.unwrap().tools, ["read_file", "bash"]);
    }

    #[test]
    fn bundle_load_reads_all_three_kinds_from_one_folder() {
        let ws = tempfile::tempdir().unwrap();
        let smedja = ws.path().join(".smedja");
        write(&smedja.join("skills/postgres-patterns/SKILL.md"), SKILL_MD);
        write(
            &smedja.join("skills/ponytail.md"),
            "# Ponytail\n\nLazy senior lens.\n",
        );
        write(
            &smedja.join("rules/no-unwrap.md"),
            "---\nname: no-unwrap\ndescription: No unwrap in library code.\n---\nbody\n",
        );
        write(&smedja.join("agents/reviewer.md"), AGENT_MD);

        let bundle = Bundle::load(ws.path(), None);
        assert_eq!(bundle.skills().count(), 2, "two skills");
        assert_eq!(bundle.rules().count(), 1, "one rule");
        assert_eq!(bundle.agents().count(), 1, "one agent");
        assert!(bundle.find("postgres-patterns").is_some());
        assert!(
            bundle.find("PONYTAIL").is_some(),
            "find is case-insensitive"
        );
    }

    #[test]
    fn bundle_load_merges_external_root() {
        let ws = tempfile::tempdir().unwrap();
        write(
            &ws.path().join(".smedja/skills/local.md"),
            "---\nname: local\ndescription: local skill.\n---\nb\n",
        );
        let ext = tempfile::tempdir().unwrap();
        write(&ext.path().join("skills/shared/SKILL.md"), SKILL_MD);
        write(&ext.path().join("agents/reviewer.md"), AGENT_MD);

        let bundle = Bundle::load(ws.path(), Some(ext.path()));
        assert!(bundle.find("local").is_some(), "workspace skill present");
        assert!(
            bundle.find("postgres-patterns").is_some(),
            "external skill present"
        );
        assert!(bundle.find("reviewer").is_some(), "external agent present");
    }

    #[test]
    fn l1_index_lists_skills_and_rules_not_agents() {
        let ws = tempfile::tempdir().unwrap();
        let smedja = ws.path().join(".smedja");
        write(&smedja.join("skills/postgres-patterns/SKILL.md"), SKILL_MD);
        write(
            &smedja.join("rules/no-unwrap.md"),
            "---\nname: no-unwrap\ndescription: No unwrap.\n---\nb\n",
        );
        write(&smedja.join("agents/reviewer.md"), AGENT_MD);

        let index = Bundle::load(ws.path(), None).l1_index().expect("index");
        assert!(index.contains("postgres-patterns"));
        assert!(index.contains("no-unwrap"));
        assert!(!index.contains("reviewer"), "agents omitted from L1 index");
    }

    #[test]
    fn empty_bundle_has_no_index() {
        let ws = tempfile::tempdir().unwrap();
        assert!(Bundle::load(ws.path(), None).l1_index().is_none());
    }

    #[test]
    fn supporting_file_paths_resolve_against_item_dir() {
        let item = parse_skill_item(SKILL_MD, Path::new("/x/y/SKILL.md"), BundleKind::Skill);
        let paths = item.supporting_file_paths();
        assert_eq!(paths, [PathBuf::from("/x/y/helpers/schema.sql")]);
    }
}
