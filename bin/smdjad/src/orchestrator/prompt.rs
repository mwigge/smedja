//! System-prompt and context-block assembly helpers for the turn orchestrator.
//!
//! These pure formatters build the discipline directive, turn-context block,
//! summariser prompt, LSP-diagnostics block, recalled-context block, and the
//! derived turn title. They hold no orchestrator state.

use smedja_assayer::AgentRole;
use smedja_vault::VaultEntry;

/// Builds the base (unsteered) system prompt for a turn, folding workspace
/// skills, the active role's skill pack, project context files, and the
/// foundational-discipline directive into one cacheable system block.
///
/// Kept unsteered so verbosity steering can be re-applied per tool-loop
/// iteration without compounding.
pub(crate) fn build_base_system(
    workspace_root: &std::path::Path,
    task_prefix: &str,
    role: AgentRole,
) -> String {
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let base = format!(
        "You are smedja, an AI coding assistant.\
        \nWorkspace: {workspace_root}\
        \nDate: {today}{task_prefix}\
        \n\nBe concise and direct. Apply the smallest diff that satisfies a \
        request. Prefer reading graph/vault context before opening files, and \
        reading files before writing them. When <recalled_context>, \
        <cold_context>, or <graph_symbols> blocks are present, treat them as \
        authoritative — reference specifics from them rather than asking the \
        user to repeat information. Ask before acting only when the request is \
        genuinely ambiguous or would be destructive.",
        workspace_root = workspace_root.display(),
    );
    let with_skills = match smedja_memory::load_workspace_skills(workspace_root) {
        Ok(skills) if !skills.is_empty() => {
            let joined = skills.join("\n\n");
            format!("{base}\n\n<workspace_skills>\n{joined}\n</workspace_skills>")
        }
        Ok(_) => base,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load workspace skills; continuing without");
            base
        }
    };
    // Role-bound rules/skills: inject the active role's pack
    // (`.smedja/roles/<role>.md` / `roles/<role>/*.md`) so each role
    // carries its own discipline (e.g. review checklist, research
    // source-hygiene, planning rules).
    let with_skills = match smedja_memory::load_role_skills(workspace_root, role.label()) {
        Ok(role_skills) if !role_skills.is_empty() => {
            let joined = role_skills.join("\n\n");
            format!(
                "{with_skills}\n\n<role_skills role=\"{}\">\n{joined}\n</role_skills>",
                role.label()
            )
        }
        Ok(_) => with_skills,
        Err(e) => {
            tracing::warn!(error = %e, role = role.label(), "failed to load role skills; continuing without");
            with_skills
        }
    };
    // Project-specific context files from `.smedja/context/*.md` are
    // injected here so they ride the stable (cacheable) system block.
    let with_skills = match smedja_memory::load_context_files(workspace_root) {
        Ok(files) if !files.is_empty() => {
            let joined = files.join("\n\n");
            format!("{with_skills}\n\n<project_context>\n{joined}\n</project_context>")
        }
        Ok(_) => with_skills,
        Err(e) => {
            tracing::warn!(error = %e, "failed to load context files; continuing without");
            with_skills
        }
    };
    // The repo's own `AGENTS.md` (the convention codex-cli/other agents read) is
    // consumed here so smedja honours it too — closing the loop in both
    // directions. The smedja-managed section is stripped first so the block the
    // codex adapter writes into `AGENTS.md` is never fed back into the prompt.
    let with_skills = match smedja_memory::detect_agents_md(workspace_root) {
        Ok(Some(body)) => {
            let user = smedja_memory::strip_managed_agents_section(&body);
            if user.trim().is_empty() {
                with_skills
            } else {
                format!("{with_skills}\n\n<agents_md>\n{user}\n</agents_md>")
            }
        }
        Ok(None) => with_skills,
        Err(e) => {
            tracing::warn!(error = %e, "failed to read AGENTS.md; continuing without");
            with_skills
        }
    };
    // Always-on, steer-first foundational discipline: the directive is
    // folded into the same cacheable system block as workspace skills so
    // it is sealed into the stable prefix before `seal_prefix()` and the
    // agent is reminded of the discipline on every code-writing turn.
    // Config-gated per discipline; omitted entirely when both are off.
    let methodology_config = crate::methodology_config::load_methodology_config(workspace_root);
    let is_rust_workspace = workspace_root.join("Cargo.toml").exists();
    match methodology_directive_for(methodology_config, is_rust_workspace) {
        Some(directive) => format!("{with_skills}\n\n{directive}"),
        None => with_skills,
    }
}

/// Builds the always-on foundational-discipline directive for the sealed system
/// prefix, gated by `config`.
///
/// TDD and clean-code discipline are steer-first: the directive is injected into
/// the cacheable system block on every code-writing turn so the agent is reminded
/// of the discipline every turn (the primary enforcement), with the diff backstop
/// secondary. Each discipline's clause is present only when its config flag is
/// `true`; when both are disabled the directive is omitted entirely (`None`).
///
/// Language-aware variant: when `is_rust` is false, Rust-specific idioms are
/// replaced with generic equivalents so the directive is not actively misleading
/// in Python, TypeScript, or other workspaces.
#[must_use]
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

/// Builds the prompt sent to the LLM to produce a conversation summary.
///
/// At most 20 turns are included; older turns are dropped from the head.
pub(crate) fn build_summariser_prompt(history: &[(String, String)]) -> String {
    const MAX_TURNS: usize = 20;
    let turns: Vec<_> = history.iter().rev().take(MAX_TURNS).collect();
    let turns_text: String = turns
        .into_iter()
        .rev()
        .map(|(role, content)| format!("{role}: {content}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Produce a structured summary of the conversation so far. \
Format it as three clearly labelled sections using bullet points:\n\
- **Decisions**: key choices made and their rationale\n\
- **Changed files**: files created, edited, or deleted (with brief reason)\n\
- **Open questions**: unresolved issues or follow-up items\n\
Omit sections that have no content. Keep total length under 400 words.\n\n\
{turns_text}"
    )
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
    use super::build_base_system;
    use smedja_assayer::AgentRole;

    fn temp_ws(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "smedja-prompt-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn build_base_system_consumes_repo_agents_md() {
        let ws = temp_ws("agents");
        std::fs::write(
            ws.join("AGENTS.md"),
            "# Repo rules\n\nNever force-push main.\n",
        )
        .unwrap();
        let out = build_base_system(&ws, "", AgentRole::Impl);
        assert!(
            out.contains("Never force-push main."),
            "repo AGENTS.md must be folded into the system block; got:\n{out}"
        );
        assert!(out.contains("<agents_md>"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn build_base_system_strips_smedja_managed_agents_section() {
        let ws = temp_ws("agents-managed");
        // Simulate an AGENTS.md the codex adapter has written a managed block into.
        let body = format!(
            "# Repo rules\n\nBe kind.\n\n{}\nYou are smedja (injected block — must NOT feed back)\n{}\n",
            smedja_memory::AGENTS_MANAGED_BEGIN,
            smedja_memory::AGENTS_MANAGED_END
        );
        std::fs::write(ws.join("AGENTS.md"), body).unwrap();
        let out = build_base_system(&ws, "", AgentRole::Impl);
        assert!(out.contains("Be kind."), "user content must be kept");
        assert!(
            !out.contains("must NOT feed back"),
            "smedja-managed section must be stripped before injection; got:\n{out}"
        );
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn build_base_system_without_agents_md_has_no_block() {
        let ws = temp_ws("no-agents");
        let out = build_base_system(&ws, "", AgentRole::Impl);
        assert!(!out.contains("<agents_md>"));
        let _ = std::fs::remove_dir_all(&ws);
    }
}
