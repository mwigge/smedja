//! `smj local` — local-model management: install, list, GPU inspect, and hot-swap.

use std::path::Path;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;

use crate::util::connect;

#[derive(Subcommand)]
pub(crate) enum LocalCmd {
    /// List the local-model inventory with GPU fit annotations
    List {
        /// Emit the raw `local.models` JSON response
        #[arg(long)]
        json: bool,
    },
    /// Show the cached GPU snapshot
    Gpu {
        /// Emit the raw `local.gpu` JSON response
        #[arg(long)]
        json: bool,
    },
    /// Hot-swap the active local model (no daemon restart)
    Swap {
        /// The model id to make active
        model: String,
    },
    /// Install a local model via the external installer (rs-llmctl)
    Install {
        /// The model id to install
        model: String,
    },
}

/// Dispatches a `smj local` subcommand.
pub(crate) async fn run(sock: &Path, action: LocalCmd) -> Result<()> {
    let mut client = connect(sock).await?;
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

/// Renders the `local.models` response as a fixed-width table with fit annotations.
///
/// Returns a header row followed by one row per model (id, est VRAM, fit, active
/// marker), or a single notice line when the inventory is empty.
fn format_local_models(resp: &serde_json::Value) -> Vec<String> {
    let Some(models) = resp.get("models").and_then(|m| m.as_array()) else {
        return vec!["no local models".to_owned()];
    };
    if models.is_empty() {
        return vec!["no local models".to_owned()];
    }
    let mut lines = vec![format!(
        "{:<32}  {:>10}  {:<8}  {}",
        "MODEL", "EST_VRAM", "FIT", "ACTIVE"
    )];
    for m in models {
        let id = m["id"].as_str().unwrap_or("-");
        let vram = m["est_vram_mb"]
            .as_u64()
            .map_or_else(|| "-".to_owned(), |v| format!("{v} MiB"));
        let fit = m["fit"].as_str().unwrap_or("unknown");
        let active = if m["active"].as_bool().unwrap_or(false) {
            "*"
        } else {
            ""
        };
        lines.push(format!("{id:<32}  {vram:>10}  {fit:<8}  {active}"));
    }
    lines
}

/// Renders the `local.gpu` response as a single human-readable line.
fn format_local_gpu(resp: &serde_json::Value) -> String {
    if !resp["detected"].as_bool().unwrap_or(false) {
        return "no GPU detected".to_owned();
    }
    let device = resp["device"].as_str().unwrap_or("unknown");
    let total = resp["vram_total_mb"].as_u64().unwrap_or(0);
    let free = resp["vram_free_mb"].as_u64().unwrap_or(0);
    format!("{device}  VRAM {free}/{total} MiB free")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Cmd};
    use clap::Parser as _;

    #[test]
    fn local_list_parses_subcommand() {
        let cli = Cli::try_parse_from(["smj", "local", "list"]).expect("local list must parse");
        match cli.command {
            Cmd::Local {
                action: LocalCmd::List { json },
            } => assert!(!json, "json defaults off"),
            _ => panic!("expected Cmd::Local List"),
        }
    }

    #[test]
    fn local_swap_parses_model_arg() {
        let cli = Cli::try_parse_from(["smj", "local", "swap", "qwen3-14b"])
            .expect("local swap must parse");
        match cli.command {
            Cmd::Local {
                action: LocalCmd::Swap { model },
            } => assert_eq!(model, "qwen3-14b"),
            _ => panic!("expected Cmd::Local Swap"),
        }
    }

    #[test]
    fn local_install_parses_model_arg() {
        let cli = Cli::try_parse_from(["smj", "local", "install", "llama3-8b"])
            .expect("local install must parse");
        match cli.command {
            Cmd::Local {
                action: LocalCmd::Install { model },
            } => assert_eq!(model, "llama3-8b"),
            _ => panic!("expected Cmd::Local Install"),
        }
    }

    #[test]
    fn format_local_models_renders_inventory_with_fit() {
        let resp = json!({
            "active_model_id": "qwen3-14b",
            "models": [
                { "id": "qwen3-14b", "est_vram_mb": 9000, "fit": "fits", "active": true },
                { "id": "huge-70b", "est_vram_mb": 48000, "fit": "exceeds", "active": false },
                { "id": "no-meta", "est_vram_mb": null, "fit": "unknown", "active": false }
            ]
        });
        let lines = format_local_models(&resp);
        assert_eq!(lines.len(), 4, "header + three models");
        assert!(lines[0].contains("MODEL") && lines[0].contains("FIT"));
        assert!(lines[1].contains("qwen3-14b") && lines[1].contains("fits"));
        assert!(lines[1].contains('*'), "active model must be marked");
        assert!(lines[2].contains("exceeds"));
        assert!(lines[3].contains("unknown") && lines[3].contains('-'));
    }

    #[test]
    fn format_local_models_empty_inventory_notice() {
        let resp = json!({ "models": [] });
        let lines = format_local_models(&resp);
        assert_eq!(lines, vec!["no local models".to_owned()]);
    }

    #[test]
    fn format_local_gpu_renders_detected_and_absent() {
        let detected = json!({
            "device": "RTX 4090", "vram_total_mb": 24000, "vram_free_mb": 20000, "detected": true
        });
        let line = format_local_gpu(&detected);
        assert!(line.contains("RTX 4090") && line.contains("20000/24000"));

        let absent = json!({ "device": null, "detected": false });
        assert_eq!(format_local_gpu(&absent), "no GPU detected");
    }
}
