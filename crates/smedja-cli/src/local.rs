use super::*;

pub(crate) async fn dispatch_local(action: LocalCmd, sock: &std::path::Path) -> Result<()> {
    let mut client = Client::connect(sock)
        .await
        .with_context(|| format!("smdjad not running ({})", sock.display()))?;
    match action {
        LocalCmd::List { json } => {
            let resp = client
                .call("local.models", json!({}))
                .await
                .context("local.models failed")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                for line in format_local_models(&resp) {
                    println!("{line}");
                }
            }
        }
        LocalCmd::Gpu { json } => {
            let resp = client
                .call("local.gpu", json!({}))
                .await
                .context("local.gpu failed")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&resp)?);
            } else {
                println!("{}", format_local_gpu(&resp));
            }
        }
        LocalCmd::Swap { model } => {
            let resp = client
                .call("local.swap", json!({ "model": model }))
                .await
                .context("local.swap failed")?;
            let active = resp["active_model_id"].as_str().unwrap_or(&model);
            let latency = resp["swap_latency_ms"].as_u64().unwrap_or(0);
            let explicit = resp["explicit_swap"].as_bool().unwrap_or(false);
            let path = if explicit {
                "explicit swap"
            } else {
                "label fallback"
            };
            println!("swapped to {active} via {path} ({latency} ms)");
        }
        LocalCmd::Install { model } => {
            let resp = client
                .call("local.install", json!({ "model": model }))
                .await
                .context("local.install failed")?;
            let installed = resp["installed"].as_bool().unwrap_or(false);
            if installed {
                println!("installed {model} (verified in inventory)");
            } else {
                let installer_ok = resp["installer_ok"].as_bool().unwrap_or(false);
                let present = resp["present_in_inventory"].as_bool().unwrap_or(false);
                println!(
                    "install of {model} not verified \
                     (installer_ok={installer_ok}, present_in_inventory={present})"
                );
            }
        }
    }
    Ok(())
}
