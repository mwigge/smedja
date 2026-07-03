//! Assorted native tools that do not warrant a module of their own:
//! `graph_query`, `load_skill`, and `alert_list`.

use serde_json::Value;

/// Queries the workspace symbol graph, returning matching symbols as JSON.
///
/// A missing `graph.db` resolves to an empty result rather than an error.
pub(crate) fn graph_query(input: &Value, workspace: &std::path::Path) -> String {
    let query = input.get("query").and_then(Value::as_str).unwrap_or("");
    let depth = u8::try_from(input.get("depth").and_then(Value::as_u64).unwrap_or(2)).unwrap_or(2);
    let graph_db_path = crate::handlers::graph::graph_db_path(workspace);
    if !graph_db_path.exists() {
        tracing::debug!("graph.db not found; returning empty symbols");
        return serde_json::json!({ "symbols": [] }).to_string();
    }
    match smedja_graph::GraphStore::open(&graph_db_path) {
        Ok(store) => match store.graph_query(query, 10, depth) {
            Ok(symbols) => {
                let sym_json: Vec<serde_json::Value> = symbols
                    .iter()
                    .map(|s| {
                        serde_json::json!({
                            "name": s.name,
                            "kind": s.kind.as_str(),
                            "file": s.file_path,
                            "line": s.start_line,
                            "snippet": s.snippet,
                        })
                    })
                    .collect();
                serde_json::json!({ "symbols": sym_json }).to_string()
            }
            Err(e) => {
                tracing::warn!(error = %e, "graph_query error");
                serde_json::json!({ "symbols": [], "error": e.to_string() }).to_string()
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "failed to open graph store");
            serde_json::json!({ "symbols": [] }).to_string()
        }
    }
}

/// Handles the `load_skill` tool: resolves the skill name from `input` and loads
/// it from the default skills directory.
pub(crate) fn load_skill(input: &Value) -> String {
    let name = input.get("name").and_then(Value::as_str).unwrap_or("");
    execute_load_skill(name, &smedja_plugins::SkillRegistry::default_path())
}

/// Load and return a named skill from `skills_dir`, wrapped in an XML envelope.
pub(crate) fn execute_load_skill(name: &str, skills_dir: &std::path::Path) -> String {
    let registry = smedja_plugins::SkillRegistry::new(skills_dir);
    match registry.find(name) {
        Ok(Some(skill)) => smedja_plugins::wrap_skill_body(&skill.manifest.name, &skill.body),
        Ok(None) => format!(
            "error: skill '{name}' not found in {}",
            skills_dir.display()
        ),
        Err(e) => format!("error: skill registry error: {e}"),
    }
}

/// Drains up to 50 pending alerts as a JSON array.
pub(crate) async fn alert_list() -> String {
    let alerts = crate::alert::drain_alerts(50).await;
    serde_json::to_string(&alerts).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::execute_load_skill;

    #[tokio::test]
    async fn load_skill_returns_wrapped_body_for_installed_skill() {
        let tmp = tempfile::tempdir().expect("tmp");
        let skills_dir = tmp.path().to_path_buf();
        // Write a minimal flat skill file.
        let skill_content = "---\nname: myskill\ndescription: A test skill.\n---\nDo the thing.\n";
        std::fs::write(skills_dir.join("myskill.md"), skill_content).unwrap();

        let result = execute_load_skill("myskill", &skills_dir);
        assert!(result.contains("Do the thing."), "body must be present");
        assert!(result.contains("<skill_content"), "must be wrapped");
    }

    #[tokio::test]
    async fn load_skill_returns_error_for_missing_skill() {
        let tmp = tempfile::tempdir().expect("tmp");
        let result = execute_load_skill("nonexistent", tmp.path());
        assert!(
            result.starts_with("error:"),
            "missing skill must return error"
        );
    }
}
