//! Pure prompt- and context-block helpers for the turn orchestrator.
//!
//! These functions shape the text that goes into a turn: the foundational
//! methodology directive, the derived session title, the LSP-diagnostics block,
//! Unicode-tag sanitisation, the per-turn context block, and the vault-recall
//! block. They are side-effect free and unit-tested in isolation.

use smedja_vault::VaultEntry;

/// Builds the always-on foundational-discipline directive for the sealed system
/// prefix, gated by `config`.
///
/// TDD and clean-code discipline are steer-first: the directive is injected into
/// the cacheable system block on every code-writing turn so the agent is reminded
/// of the discipline every turn (the primary enforcement), with the diff backstop
/// secondary. Each discipline's clause is present only when its config flag is
/// `true`; when both are disabled the directive is omitted entirely (`None`).
#[must_use]
/// Language-aware variant: when `is_rust` is false, Rust-specific idioms are
/// replaced with generic equivalents so the directive is not actively misleading
/// in Python, TypeScript, or other workspaces.
pub(crate) fn methodology_directive_for(
    config: smedja_methodology::MethodologyConfig,
    is_rust: bool,
) -> Option<String> {
    if !config.tdd && !config.clean {
        return None;
    }
    let mut clauses: Vec<&'static str> = Vec::new();
    if config.tdd {
        clauses.push(
            "Write a failing test before the implementation it covers (Red, then Green, \
             then Refactor); keep functions small and focused; prefer an early return over \
             an `else` branch.",
        );
    }
    if config.clean {
        if is_rust {
            clauses.push(
                "Do not use `unwrap`, `expect`, or `println!` in library code — return errors \
                 with `?` and log through the structured logger.",
            );
        } else {
            clauses.push(
                "Do not swallow errors silently — propagate or log them explicitly. \
                 Avoid bare print statements in library code; use the structured logger.",
            );
        }
    }
    let body = clauses
        .iter()
        .map(|c| format!("- {c}"))
        .collect::<Vec<_>>()
        .join("\n");
    Some(format!(
        "<methodology_discipline>\n{body}\n</methodology_discipline>"
    ))
}

/// Derives a short title (≤10 words) from raw user turn content.
///
/// Strips any auto-injected context blocks (e.g. `<graph_symbols>`) that start
/// after a blank line and takes the first ten whitespace-separated words of the
/// remaining text.
pub(crate) fn derive_title(content: &str) -> String {
    let clean = content.split("\n\n<").next().unwrap_or(content).trim();
    clean
        .split_whitespace()
        .take(10)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Formats the current LSP snapshot into a `<lsp_diagnostics>` context block.
///
/// Returns `None` when there are no errors or warnings (info / hints are skipped).
/// At most 20 diagnostic lines are included; a trailing note is appended when
/// the list is truncated.
pub(crate) fn format_lsp_diagnostics(snapshot: &smedja_lsp::LspSnapshot) -> Option<String> {
    use smedja_lsp::types::Severity;
    const MAX: usize = 20;
    let relevant: Vec<_> = snapshot
        .diagnostics
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error | Severity::Warning))
        .collect();
    if relevant.is_empty() {
        return None;
    }
    let mut lines: Vec<String> = relevant
        .iter()
        .take(MAX)
        .map(|d| {
            let sev = match d.severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
                _ => "info",
            };
            let code = d
                .code
                .as_deref()
                .map_or_else(String::new, |c| format!(" [{c}]"));
            format!(
                "{} {}:{}: {}{}",
                sev,
                d.file.display(),
                d.line,
                d.message,
                code
            )
        })
        .collect();
    if relevant.len() > MAX {
        lines.push(format!(
            "... and {} more (only the first {MAX} shown)",
            relevant.len() - MAX
        ));
    }
    Some(format!(
        "<lsp_diagnostics>\n{}\n</lsp_diagnostics>",
        lines.join("\n")
    ))
}

/// Strips Unicode Private Use Area characters in the tag block (U+E0000–U+E007F).
///
/// These code points can be used to inject hidden instructions via Unicode tag
/// characters that are visually invisible but parsed by some LLM tokenizers.
pub(crate) fn sanitize_unicode_tags(s: &str) -> String {
    s.chars()
        .filter(|&c| !('\u{E0000}'..='\u{E007F}').contains(&c))
        .collect()
}

/// Builds a `<turn-context>` XML block injected at the start of each user turn.
///
/// The block carries per-turn metadata (current date, working directory) that
/// helps the model orient itself without polluting the stable system-prompt
/// prefix used for prompt cache hits.
pub(crate) fn build_turn_context(date: &str, cwd: &str) -> String {
    format!(
        "<turn-context>\n<current-date>{date}</current-date>\n<working-directory>{cwd}</working-directory>\n</turn-context>"
    )
}

/// Formats vault recall results as a `<recalled_context>` XML block for injection
/// into the user turn. Returns `None` when `entries` is empty.
pub(crate) fn format_vault_recalled(entries: &[VaultEntry]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }
    let body = entries
        .iter()
        .map(|e| e.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    Some(format!("<recalled_context>\n{body}\n</recalled_context>"))
}

#[cfg(test)]
mod tests {
    use smedja_methodology::MethodologyConfig;

    #[test]
    fn directive_present_under_default_config() {
        // On a code-writing turn with default config the sealed system prefix
        // carries the TDD/clean discipline directive (both clauses present).
        let directive = super::methodology_directive_for(MethodologyConfig::default(), true)
            .expect("default config must yield a directive");
        assert!(directive.contains("<methodology_discipline>"));
        assert!(directive.contains("failing test"));
        assert!(directive.contains("`unwrap`"));
    }

    #[test]
    fn tdd_clause_omitted_when_tdd_disabled() {
        let cfg = MethodologyConfig {
            tdd: false,
            clean: true,
        };
        let directive = super::methodology_directive_for(cfg, true)
            .expect("clean clause must still be present");
        assert!(!directive.contains("failing test"));
        assert!(directive.contains("`unwrap`"));
    }

    #[test]
    fn clean_clause_omitted_when_clean_disabled() {
        let cfg = MethodologyConfig {
            tdd: true,
            clean: false,
        };
        let directive =
            super::methodology_directive_for(cfg, true).expect("tdd clause must still be present");
        assert!(directive.contains("failing test"));
        assert!(!directive.contains("`unwrap`"));
    }

    #[test]
    fn directive_omitted_entirely_when_both_disabled() {
        let cfg = MethodologyConfig {
            tdd: false,
            clean: false,
        };
        assert!(super::methodology_directive_for(cfg, true).is_none());
    }

    // --- derive_title tests ---

    #[test]
    fn derive_title_takes_first_ten_words() {
        let input = "one two three four five six seven eight nine ten eleven twelve";
        let title = super::derive_title(input);
        assert_eq!(title, "one two three four five six seven eight nine ten");
    }

    #[test]
    fn derive_title_short_input_unchanged() {
        let title = super::derive_title("fix the bug");
        assert_eq!(title, "fix the bug");
    }

    #[test]
    fn derive_title_strips_graph_injection_block() {
        let input = "refactor auth module\n\n<graph_symbols>\nsome code\n</graph_symbols>";
        let title = super::derive_title(input);
        assert_eq!(title, "refactor auth module");
    }

    #[test]
    fn derive_title_empty_input_returns_empty() {
        assert_eq!(super::derive_title(""), "");
    }

    // --- format_lsp_diagnostics tests ---

    #[test]
    fn format_lsp_diagnostics_empty_snapshot_returns_none() {
        let snap = smedja_lsp::LspSnapshot::default();
        assert!(super::format_lsp_diagnostics(&snap).is_none());
    }

    #[test]
    fn format_lsp_diagnostics_errors_and_warnings_included() {
        use smedja_lsp::types::{Diagnostic, Severity};
        use std::path::PathBuf;
        let snap = smedja_lsp::LspSnapshot {
            servers: vec![],
            diagnostics: vec![
                Diagnostic {
                    file: PathBuf::from("src/main.rs"),
                    line: 42,
                    col: 1,
                    severity: Severity::Error,
                    code: Some("E0308".to_owned()),
                    message: "mismatched types".to_owned(),
                },
                Diagnostic {
                    file: PathBuf::from("src/lib.rs"),
                    line: 17,
                    col: 5,
                    severity: Severity::Warning,
                    code: None,
                    message: "unused variable".to_owned(),
                },
            ],
        };
        let block = super::format_lsp_diagnostics(&snap).unwrap();
        assert!(block.contains("<lsp_diagnostics>"));
        assert!(block.contains("src/main.rs:42"));
        assert!(block.contains("mismatched types"));
        assert!(block.contains("src/lib.rs:17"));
        assert!(block.contains("unused variable"));
    }

    #[test]
    fn format_lsp_diagnostics_caps_at_twenty_lines() {
        use smedja_lsp::types::{Diagnostic, Severity};
        use std::path::PathBuf;
        let diags: Vec<Diagnostic> = (0..30)
            .map(|i| Diagnostic {
                file: PathBuf::from("src/main.rs"),
                line: i,
                col: 1,
                severity: Severity::Error,
                code: None,
                message: format!("err {i}"),
            })
            .collect();
        let snap = smedja_lsp::LspSnapshot {
            servers: vec![],
            diagnostics: diags,
        };
        let block = super::format_lsp_diagnostics(&snap).unwrap();
        let lines: Vec<&str> = block.lines().collect();
        // header + up to 20 diag lines + footer + optional truncation line
        assert!(lines.len() <= 23, "too many lines: {}", lines.len());
    }

    // --- format_vault_recalled tests ---

    fn make_vault_entry(content: &str) -> smedja_vault::VaultEntry {
        smedja_vault::VaultEntry {
            id: "test-id".into(),
            embedding: vec![0.1; 128],
            payload: serde_json::Value::Null,
            namespace: "default".into(),
            content: content.into(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: "fnv-bow-128".into(),
            dim: 128,
        }
    }

    #[test]
    fn format_vault_recalled_empty_returns_none() {
        assert!(super::format_vault_recalled(&[]).is_none());
    }

    #[test]
    fn format_vault_recalled_single_entry_wraps_in_xml() {
        let entries = vec![make_vault_entry("the auth token expires after 24 hours")];
        let result = super::format_vault_recalled(&entries).unwrap();
        assert!(result.starts_with("<recalled_context>"));
        assert!(result.contains("auth token expires after 24 hours"));
        assert!(result.ends_with("</recalled_context>"));
    }

    #[test]
    fn format_vault_recalled_multiple_entries_joined_with_separator() {
        let entries = vec![make_vault_entry("note one"), make_vault_entry("note two")];
        let result = super::format_vault_recalled(&entries).unwrap();
        assert!(result.contains("note one"));
        assert!(result.contains("note two"));
        assert!(result.contains("---"));
    }

    // --- sanitize_unicode_tags tests ---

    #[test]
    fn sanitize_unicode_tags_strips_private_use_area_block() {
        // U+E0000 tag block (used for prompt injection via Unicode tags)
        let injected = "hello\u{E0001}world\u{E007F}!";
        let clean = super::sanitize_unicode_tags(injected);
        assert_eq!(clean, "helloworld!");
    }

    #[test]
    fn sanitize_unicode_tags_leaves_normal_text_intact() {
        let normal = "Hello, World! こんにちは 🦀";
        assert_eq!(super::sanitize_unicode_tags(normal), normal);
    }

    // --- build_turn_context tests ---

    #[test]
    fn build_turn_context_contains_date_and_cwd() {
        let ctx = super::build_turn_context("2026-06-30", "/home/morgan/project");
        assert!(ctx.starts_with("<turn-context>"), "must open tag");
        assert!(ctx.ends_with("</turn-context>"), "must close tag");
        assert!(ctx.contains("2026-06-30"), "must include date");
        assert!(ctx.contains("/home/morgan/project"), "must include cwd");
    }

    #[test]
    fn build_turn_context_is_stable_across_calls_same_input() {
        let a = super::build_turn_context("2026-06-30", "/repo");
        let b = super::build_turn_context("2026-06-30", "/repo");
        assert_eq!(
            a, b,
            "same inputs must produce identical output for cache stability"
        );
    }
}
