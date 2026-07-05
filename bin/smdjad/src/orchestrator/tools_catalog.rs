//! Builds the built-in tool catalog advertised to the model for a turn.
//!
//! Returns the always-present native tools plus, in `sre` mode, the
//! observability tools. The caller chains this with any MCP-provided tools.

/// Assembles the built-in tool schemas. When `is_sre_mode` is true the SRE
/// observability tools (`alert_list`, `otel_query`, `metric_query`, `log_tail`)
/// are appended.
pub(crate) fn builtin_tools(is_sre_mode: bool) -> Vec<serde_json::Value> {
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
        serde_json::json!({
            "name": "lsp_definition",
            "description": "Jump to the definition of the symbol at a source position using \
                the language server. Prefer this over text search for precise navigation in \
                indexed languages (Rust, Python, Go, TypeScript/JavaScript, C/C++). \
                Returns { locations: [{ file, line, col }] } (1-based).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "workspace-relative path" },
                    "line": { "type": "integer", "description": "1-based line" },
                    "col": { "type": "integer", "description": "1-based column, default 1" }
                },
                "required": ["file", "line"]
            }
        }),
        serde_json::json!({
            "name": "lsp_references",
            "description": "Find all references to the symbol at a source position via the \
                language server. Returns { references: [{ file, line, col }] } (1-based).",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string" },
                    "line": { "type": "integer" },
                    "col": { "type": "integer" }
                },
                "required": ["file", "line"]
            }
        }),
        serde_json::json!({
            "name": "lsp_hover",
            "description": "Get type/signature/doc hover text for the symbol at a source \
                position via the language server. Returns { hover: string }.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string" },
                    "line": { "type": "integer" },
                    "col": { "type": "integer" }
                },
                "required": ["file", "line"]
            }
        }),
        serde_json::json!({
            "name": "lsp_document_symbols",
            "description": "List the symbols (functions, types, methods, …) declared in a \
                file via the language server. Returns { symbols: [{ name, kind, file, line }] }.",
            "input_schema": {
                "type": "object",
                "properties": { "file": { "type": "string" } },
                "required": ["file"]
            }
        }),
        serde_json::json!({
            "name": "lsp_workspace_symbols",
            "description": "Search project-wide symbols by name via the language server. \
                Returns { symbols: [{ name, kind, file, line }] }.",
            "input_schema": {
                "type": "object",
                "properties": { "query": { "type": "string" } },
                "required": ["query"]
            }
        }),
        serde_json::json!({
            "name": "lsp_rename_symbol",
            "description": "Rename the symbol at a source position across the workspace using \
                the language server's rename support, applying the resulting edits to disk. \
                Writes are bounded to the workspace and routed through the approval gate. \
                Returns { renamed: true, changed_files: [..] }.",
            "input_schema": {
                "type": "object",
                "properties": {
                    "file": { "type": "string" },
                    "line": { "type": "integer" },
                    "col": { "type": "integer" },
                    "new_name": { "type": "string" }
                },
                "required": ["file", "line", "new_name"]
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
