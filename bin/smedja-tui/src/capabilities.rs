/// Returns `true` when `runner` supports extended thinking tokens.
pub(crate) fn runner_supports_thinking(runner: &str) -> bool {
    matches!(runner, "anthropic")
}

/// Returns `true` when `runner` is a subprocess CLI wrapper rather than a
/// native HTTP provider.
pub(crate) fn runner_is_subprocess(runner: &str) -> bool {
    matches!(runner, "claude-cli" | "codex-cli")
}

/// Formats a capability table from a `runner.list` response array.
///
/// Each row shows runner name, tier, model, and derived capability flags
/// (thinking support, subprocess mode).
pub(crate) fn format_capabilities_table(runners: &[serde_json::Value]) -> String {
    if runners.is_empty() {
        return "no runners available".to_owned();
    }
    let mut lines = vec![format!(
        "{:<16} {:<8} {:<8} {:<36}",
        "runner", "tier", "flags", "model"
    )];
    lines.push("-".repeat(72));
    for r in runners {
        let name = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
        let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("-");
        let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("-");
        let mut flags: Vec<&str> = Vec::new();
        if runner_supports_thinking(name) {
            flags.push("thinking");
        }
        if runner_is_subprocess(name) {
            flags.push("subprocess");
        }
        let flag_str = if flags.is_empty() {
            "-".to_owned()
        } else {
            flags.join(",")
        };
        lines.push(format!("{name:<16} {tier:<8} {flag_str:<8} {model}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use crate::testutil::{make_state, render_frame};
    #[allow(unused_imports)]
    use serde_json::{json, Value};

    #[test]
    fn runner_capability_flags_for_known_runners() {
        assert!(runner_supports_thinking("anthropic"));
        assert!(!runner_supports_thinking("claude-cli"));
        assert!(!runner_supports_thinking("openai"));
        assert!(runner_is_subprocess("claude-cli"));
        assert!(runner_is_subprocess("codex-cli"));
        assert!(!runner_is_subprocess("anthropic"));
    }

    #[test]
    fn format_capabilities_table_lists_runners() {
        let runners = vec![
            serde_json::json!({ "runner": "anthropic", "tier": "fast", "model": "claude-haiku-4-5-20251001" }),
            serde_json::json!({ "runner": "claude-cli", "tier": "fast", "model": "claude-opus" }),
        ];
        let table = format_capabilities_table(&runners);
        assert!(table.contains("anthropic"), "{table}");
        assert!(table.contains("thinking"), "{table}");
        assert!(table.contains("subprocess"), "{table}");
    }
}
