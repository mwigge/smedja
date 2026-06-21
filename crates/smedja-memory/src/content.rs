//! Typed content blocks for structured prompt assembly.
//!
//! [`MessageContent`] provides semantic variants for the different kinds of
//! content that appear in a smedja agent prompt — plain text, file context
//! (possibly an outline), skill bodies, code-graph chunks, and unified diffs.
//!
//! [`render_message_content`] collapses a `&[MessageContent]` slice into a
//! single [`String`] using XML-fenced sections, suitable for inclusion in a
//! provider message.

use std::path::PathBuf;

/// A typed content block for structured prompt assembly.
#[derive(Debug, Clone)]
pub enum MessageContent {
    /// Plain text, rendered inline without any wrapping.
    Text(String),

    /// A file's content, possibly replaced by a tree-sitter outline when the
    /// file is larger than the inline threshold.
    FileContext {
        /// Path to the source file.
        path: PathBuf,
        /// File content or outline text.
        content: String,
        /// `true` when `content` is a tree-sitter outline rather than the full
        /// file body.
        is_outline: bool,
    },

    /// A skill body loaded from the skill registry.
    SkillBody {
        /// Skill name (matches the manifest `name` field).
        name: String,
        /// Skill body text (may be XML-escaped by the skill envelope layer).
        body: String,
    },

    /// A slice of symbols returned by a code-graph query.
    CodeGraphChunk {
        /// Symbol representations (e.g. `"fn foo (src/lib.rs:12)"`).
        symbols: Vec<String>,
    },

    /// A unified diff for a single file.
    Diff {
        /// Path to the file the diff applies to.
        path: PathBuf,
        /// Unified diff text.
        unified: String,
    },
}

/// Renders a slice of [`MessageContent`] blocks into a single `String`.
///
/// Rendering rules:
/// - [`MessageContent::Text`] → appended inline without wrapping
/// - [`MessageContent::FileContext`] → `<file path="...">content</file>`,
///   with `[outline]` prepended to `content` when `is_outline` is `true`
/// - [`MessageContent::SkillBody`] → `<skill name="...">body</skill>`
/// - [`MessageContent::CodeGraphChunk`] →
///   `<code_graph>\nsymbol1\n...\n</code_graph>`
/// - [`MessageContent::Diff`] → `<diff path="...">unified</diff>`
#[must_use]
pub fn render_message_content(parts: &[MessageContent]) -> String {
    let mut out = String::new();
    for part in parts {
        match part {
            MessageContent::Text(s) => {
                out.push_str(s);
            }
            MessageContent::FileContext {
                path,
                content,
                is_outline,
            } => {
                out.push_str(&format!(
                    "<file path=\"{}\">{}{}</file>",
                    path.display(),
                    if *is_outline { "[outline]\n" } else { "" },
                    content,
                ));
            }
            MessageContent::SkillBody { name, body } => {
                out.push_str(&format!("<skill name=\"{name}\">{body}</skill>"));
            }
            MessageContent::CodeGraphChunk { symbols } => {
                out.push_str("<code_graph>\n");
                for sym in symbols {
                    out.push_str(sym);
                    out.push('\n');
                }
                out.push_str("</code_graph>");
            }
            MessageContent::Diff { path, unified } => {
                out.push_str(&format!(
                    "<diff path=\"{}\">{unified}</diff>",
                    path.display(),
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_renders_inline() {
        let parts = vec![MessageContent::Text("hello world".to_owned())];
        assert_eq!(render_message_content(&parts), "hello world");
    }

    #[test]
    fn file_context_renders_with_file_tags() {
        let parts = vec![MessageContent::FileContext {
            path: PathBuf::from("src/lib.rs"),
            content: "fn main() {}".to_owned(),
            is_outline: false,
        }];
        let rendered = render_message_content(&parts);
        assert!(rendered.starts_with("<file path=\"src/lib.rs\">"));
        assert!(rendered.ends_with("</file>"));
        assert!(rendered.contains("fn main() {}"));
    }

    #[test]
    fn file_context_outline_includes_outline_tag() {
        let parts = vec![MessageContent::FileContext {
            path: PathBuf::from("large.rs"),
            content: "1: fn foo\n2: fn bar".to_owned(),
            is_outline: true,
        }];
        let rendered = render_message_content(&parts);
        assert!(rendered.contains("[outline]"));
        assert!(rendered.contains("1: fn foo"));
    }

    #[test]
    fn skill_body_renders_with_skill_tags() {
        let parts = vec![MessageContent::SkillBody {
            name: "rust".to_owned(),
            body: "# Rules\n1. No unwrap".to_owned(),
        }];
        let rendered = render_message_content(&parts);
        assert_eq!(
            rendered,
            "<skill name=\"rust\"># Rules\n1. No unwrap</skill>"
        );
    }

    #[test]
    fn code_graph_chunk_renders_with_code_graph_tags() {
        let parts = vec![MessageContent::CodeGraphChunk {
            symbols: vec![
                "fn foo (lib.rs:1)".to_owned(),
                "fn bar (lib.rs:5)".to_owned(),
            ],
        }];
        let rendered = render_message_content(&parts);
        assert!(rendered.starts_with("<code_graph>"));
        assert!(rendered.ends_with("</code_graph>"));
        assert!(rendered.contains("fn foo (lib.rs:1)"));
        assert!(rendered.contains("fn bar (lib.rs:5)"));
    }

    #[test]
    fn mixed_content_renders_in_order() {
        let parts = vec![
            MessageContent::Text("intro ".to_owned()),
            MessageContent::SkillBody {
                name: "tdd".to_owned(),
                body: "red-green-refactor".to_owned(),
            },
            MessageContent::Text(" outro".to_owned()),
        ];
        let rendered = render_message_content(&parts);
        assert_eq!(
            rendered,
            "intro <skill name=\"tdd\">red-green-refactor</skill> outro"
        );
    }

    #[test]
    fn diff_renders_with_diff_tags() {
        let parts = vec![MessageContent::Diff {
            path: PathBuf::from("src/main.rs"),
            unified: "@@ -1,1 +1,2 @@\n+fn added() {}".to_owned(),
        }];
        let rendered = render_message_content(&parts);
        assert!(rendered.starts_with("<diff path=\"src/main.rs\">"));
        assert!(rendered.ends_with("</diff>"));
    }
}
