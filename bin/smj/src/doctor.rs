//! `smj doctor` — provider health check (active runners, kind, env var status).

use std::path::Path;

use anyhow::Result;
use smedja_rpc::client::Client;

/// Prints the provider pool status — which runners are active, whether they
/// use native HTTP or a subprocess CLI binary, and which env vars are set.
/// When `--json` is passed, emits a machine-readable JSON object instead.
pub(crate) async fn run(sock: &Path, json: bool) -> Result<()> {
    let key_status = |var: &str| {
        if std::env::var(var).is_ok_and(|v| !v.trim().is_empty()) {
            "set"
        } else {
            "unset"
        }
    };

    let env_vars = [
        ("ANTHROPIC_API_KEY", "anthropic / claude-cli"),
        ("OPENAI_API_KEY", "openai / codex-cli"),
        ("GITHUB_TOKEN", "copilot"),
        ("MINIMAX_API_KEY", "minimax"),
        ("BERGET_API_KEY", "berget"),
        ("GROQ_API_KEY", "groq"),
        ("DEEPSEEK_API_KEY", "deepseek"),
        ("TOGETHER_API_KEY", "together"),
        ("PERPLEXITY_API_KEY", "perplexity"),
        ("XAI_API_KEY", "xai"),
        ("OLLAMA_HOST", "ollama"),
        ("AWS_ACCESS_KEY_ID", "bedrock"),
        ("AWS_SECRET_ACCESS_KEY", "bedrock"),
        ("AWS_DEFAULT_REGION", "bedrock"),
        ("SMEDJA_LOCAL_ENDPOINT", "local"),
    ];

    let runners: Vec<serde_json::Value> = match Client::connect(sock).await {
        Ok(mut client) => {
            let resp = client
                .call("runner.list", serde_json::Value::Null)
                .await
                .unwrap_or_default();
            resp.get("runners")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };

    if json {
        let env_json: Vec<serde_json::Value> = env_vars
            .iter()
            .map(|(var, provider)| {
                serde_json::json!({
                    "var": var,
                    "provider": provider,
                    "status": key_status(var),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "daemon": !runners.is_empty(),
                "runners": runners,
                "env": env_json,
            }))?
        );
        return Ok(());
    }

    if runners.is_empty() {
        println!("daemon: not running");
    } else {
        println!("daemon: running\n");
        println!("runner         tier   kind         model");
        println!("{}", "-".repeat(68));
        for r in &runners {
            let name = r.get("runner").and_then(|v| v.as_str()).unwrap_or("?");
            let tier = r.get("tier").and_then(|v| v.as_str()).unwrap_or("?");
            let model = r.get("model").and_then(|v| v.as_str()).unwrap_or("?");
            let kind = if is_subprocess_runner(name) {
                "subprocess"
            } else {
                "native"
            };
            println!("{name:<14} {tier:<6} {kind:<12} {model}");
        }
    }

    println!("\nenv:");
    for (var, provider) in &env_vars {
        println!("  {var:<28} {:<6}  ({provider})", key_status(var));
    }

    Ok(())
}

/// Returns `true` when `runner_name` uses a subprocess CLI binary rather than
/// the native HTTP API.
fn is_subprocess_runner(runner_name: &str) -> bool {
    matches!(runner_name, "claude-cli" | "codex-cli")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser as _;

    #[test]
    fn doctor_subcommand_parses() {
        Cli::try_parse_from(["smj", "doctor"]).expect("smj doctor must parse");
        // --json flag
        Cli::try_parse_from(["smj", "doctor", "--json"]).expect("smj doctor --json must parse");
    }

    #[test]
    fn is_subprocess_runner_distinguishes_native_from_cli() {
        assert!(is_subprocess_runner("claude-cli"));
        assert!(is_subprocess_runner("codex-cli"));
        assert!(!is_subprocess_runner("anthropic"));
        assert!(!is_subprocess_runner("openai"));
        assert!(!is_subprocess_runner("copilot"));
        assert!(!is_subprocess_runner("minimax"));
        assert!(!is_subprocess_runner("berget"));
        assert!(!is_subprocess_runner("local"));
    }
}
