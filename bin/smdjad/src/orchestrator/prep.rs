//! Per-turn prompt assembly: the sealed system prompt, the builtin tool
//! catalogue, and the first user-turn content (with auto-injected graph symbols
//! and LSP diagnostics). Extracted from `TurnOrchestrator::run` so the turn
//! pipeline stays readable; each function is a pure builder over explicit inputs.

use std::fmt::Write as _;
use std::path::Path;

use smedja_assayer::AgentRole;
use smedja_lsp::LspManager;

/// Builds the base system prompt with workspace skills, role skills, project
/// context files, and the foundational-discipline directive folded into the
/// stable (cacheable) system block.
///
/// `base_system` is kept unsteered so verbosity steering can be re-applied per
/// tool-loop iteration without compounding.
pub(crate) fn build_base_system(
    workspace_root: &Path,
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
    // Always-on, steer-first foundational discipline: the directive is
    // folded into the same cacheable system block as workspace skills so
    // it is sealed into the stable prefix before `seal_prefix()` and the
    // agent is reminded of the discipline on every code-writing turn.
    // Config-gated per discipline; omitted entirely when both are off.
    let methodology_config = crate::methodology_config::load_methodology_config(workspace_root);
    let is_rust_workspace = workspace_root.join("Cargo.toml").exists();
    match super::prompt::methodology_directive_for(methodology_config, is_rust_workspace) {
        Some(directive) => format!("{with_skills}\n\n{directive}"),
        None => with_skills,
    }
}

/// Builds the builtin tool catalogue advertised to the model.
///
/// When `is_sre_mode` is set, the SRE-only observability tools (alerts, otel,
/// metrics, logs) are appended.
pub(crate) fn build_builtin_tools(is_sre_mode: bool) -> Vec<serde_json::Value> {
    let mut builtin_tools: Vec<serde_json::Value> = vec![
        serde_json::json!({
            "name": "smedja_vault_search",
            "description": "Search the smedja vault for semantically similar entries. \
                namespace: optional — defaults to 'default'; use 'compact' for session \
                summaries, or the role label (e.g. 'review', 'sre') for role-scoped knowledge. \
                k: number of results to return, default 3.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "namespace": { "type": "string", "description": "defaults to 'default'; known values: compact, default, review, sre, researcher" },
                    "k": { "type": "integer", "description": "number of results, default 3" }
                },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": "smedja_vault_store",
            "description": "Store an entry in the smedja vault for future retrieval. \
                namespace: optional — defaults to 'default'; use 'compact' for session \
                summaries, or the role label (e.g. 'review', 'sre') for role-scoped knowledge. \
                Omitting namespace stores in 'default', which is always included in proactive recall.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "namespace": { "type": "string", "description": "defaults to 'default'; known values: compact, default, review, sre, researcher" },
                    "id": { "type": "string" },
                    "payload": { "type": "object" },
                    "source_file": { "type": "string" },
                    "added_by": { "type": "string" }
                },
                "required": ["content"]
            }
        }),
        serde_json::json!({
            "name": "smedja_retrieve",
            "description": "Retrieve the original full content for a compressed block by its content hash.",
            "input_schema": {
                "type": "object",
                "properties": { "hash": { "type": "string" } },
                "required": ["hash"]
            }
        }),
        serde_json::json!({
            "name": "graph_query",
            "description": "Query the workspace code graph for symbols related to a query.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "depth": { "type": "integer" }
                },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": "load_skill",
            "description": "Load a skill by name and return its body wrapped in XML. \
                Use this to load a skill at runtime when invoking it via a slash command \
                or when instructed to apply a specific skill.",
            "input_schema": {
                "type": "object",
                "properties": { "name": { "type": "string", "description": "Skill name (e.g. 'rust', 'tdd-workflow')" } },
                "required": ["name"]
            }
        }),
    ];
    if is_sre_mode {
        builtin_tools.push(serde_json::json!({
            "name": "alert_list",
            "description": "Drain up to 50 pending alerts from the alert queue.",
            "input_schema": { "type": "object", "properties": {} }
        }));
        builtin_tools.push(serde_json::json!({
            "name": "otel_query",
            "description": "Query SigNoz traces API.",
            "input_schema": { "type": "object", "properties": { "service": { "type": "string" }, "filter": { "type": "string" }, "range_minutes": { "type": "integer" } }, "required": ["service"] }
        }));
        builtin_tools.push(serde_json::json!({
            "name": "metric_query",
            "description": "Query Prometheus with PromQL.",
            "input_schema": { "type": "object", "properties": { "promql": { "type": "string" }, "range_minutes": { "type": "integer" } }, "required": ["promql"] }
        }));
        builtin_tools.push(serde_json::json!({
            "name": "log_tail",
            "description": "Tail logs from Loki.",
            "input_schema": { "type": "object", "properties": { "service": { "type": "string" }, "filter": { "type": "string" }, "lines": { "type": "integer" } }, "required": ["service"] }
        }));
    }
    builtin_tools
}

/// Builds the first user-turn content: the user message with auto-injected
/// top-3 graph symbols, optional LSP diagnostics (code-focused turns only), a
/// Unicode-tag sanitisation pass, and a leading per-turn context block.
pub(crate) fn build_first_user_content(
    task_title: &str,
    workspace_root: &Path,
    role: AgentRole,
    lsp_manager: &LspManager,
) -> String {
    let mut content = task_title.to_owned();
    // Auto-inject top-3 graph symbols related to user message nouns.
    let stop_words = [
        "the", "and", "for", "with", "this", "that", "from", "into", "use", "are", "was", "has",
        "not", "can", "its", "will",
    ];
    let nouns: Vec<&str> = task_title
        .split_whitespace()
        .filter(|t| t.len() >= 3 && !stop_words.contains(&t.to_lowercase().as_str()))
        .take(5)
        .collect();
    let mut injected_count = 0usize;
    if !nouns.is_empty() {
        let graph_db_path = crate::handlers::graph::graph_db_path(workspace_root);
        if graph_db_path.exists() {
            match smedja_graph::GraphStore::open(&graph_db_path) {
                Ok(store) => {
                    let query = nouns.join(" ");
                    match store.graph_query(&query, 3, 2) {
                        Ok(symbols) => {
                            if !symbols.is_empty() {
                                let snippets: Vec<String> = symbols
                                    .iter()
                                    .map(|s| {
                                        format!(
                                            "// {} {} ({}:{})\n{}",
                                            s.kind.as_str(),
                                            s.name,
                                            s.file_path,
                                            s.start_line,
                                            s.snippet
                                        )
                                    })
                                    .collect();
                                let _ = write!(
                                    content,
                                    "\n\n<graph_symbols>\n{}\n</graph_symbols>",
                                    snippets.join("\n\n")
                                );
                                injected_count = symbols.len();
                            }
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "graph_query failed; skipping injection");
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(error = %e, "could not open graph.db; skipping injection");
                }
            }
        } else {
            tracing::debug!("graph.db not found; skipping auto-injection");
        }
    }
    tracing::debug!(
        smedja.turn.graph_symbols_injected = injected_count,
        "graph symbol injection"
    );
    // Append LSP diagnostics only when the turn is code-focused: coder
    // roles or queries that mention fix/build/compile/error keywords.
    let wants_diag = matches!(role, AgentRole::Impl | AgentRole::Debug | AgentRole::Test)
        || ["fix", "build", "compile", "error", "warn"]
            .iter()
            .any(|kw| task_title.to_lowercase().contains(kw));
    if wants_diag {
        if let Some(diag_block) = super::prompt::format_lsp_diagnostics(&lsp_manager.snapshot()) {
            let _ = write!(content, "\n\n{diag_block}");
        }
    }
    // Sanitize Unicode tag block (U+E0000–U+E007F) to block prompt injection.
    let content = super::prompt::sanitize_unicode_tags(&content);
    // Prepend per-turn context block (date + cwd) for model orientation.
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let cwd_str = workspace_root.to_string_lossy();
    let turn_ctx = super::prompt::build_turn_context(&date, &cwd_str);
    format!("{turn_ctx}\n\n{content}")
}
