use super::*;

pub(crate) fn is_subprocess_runner(runner_name: &str) -> bool {
    matches!(runner_name, "claude-cli" | "codex-cli")
}

pub(crate) async fn cmd_doctor(sock: &std::path::Path, json: bool) -> Result<()> {
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
