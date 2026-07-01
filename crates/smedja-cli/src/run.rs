use super::*;
use crate::audit::dispatch_audit;
use crate::governance::dispatch_gov;
use crate::local::dispatch_local;
use crate::loop_cmd::dispatch_loop;
use crate::sessions::dispatch_session;
use crate::tasks::dispatch_task;
use crate::timeline::dispatch_timeline;
use crate::usage::{dispatch_cost, dispatch_metrics, dispatch_savings};
use crate::workspace::dispatch_workspace;

pub async fn run() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let sock = cli.sock.unwrap_or_else(default_socket_path);

    match cli.command {
        Cmd::Daemon { action } => match action {
            DaemonCmd::Status => cmd_daemon_status(&sock).await?,
            DaemonCmd::Start => cmd_daemon_start()?,
            DaemonCmd::Stop => {
                let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
                let pid_path = std::path::PathBuf::from(base).join("smdjad.pid");
                let pid = std::fs::read_to_string(&pid_path)
                    .context("smdjad not running (no PID file)")?
                    .trim()
                    .to_owned();
                std::process::Command::new("kill")
                    .args(["-TERM", &pid])
                    .status()
                    .context("kill -TERM failed")?;
                println!("smdjad stopped (pid {pid})");
            }
            DaemonCmd::Restart => {
                let base = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
                let pid_path = std::path::PathBuf::from(base).join("smdjad.pid");
                if let Ok(pid) = std::fs::read_to_string(&pid_path).map(|s| s.trim().to_owned()) {
                    let _ = std::process::Command::new("kill")
                        .args(["-TERM", &pid])
                        .status();
                    wait_for_daemon_exit(&pid, &sock)
                        .context("old smdjad did not shut down cleanly")?;
                }
                cmd_daemon_start()?;
                println!("smdjad restarted");
            }
        },
        Cmd::Skill { action } => {
            let registry = SkillRegistry::new(SkillRegistry::default_path());
            match action {
                SkillCmd::List => cmd_skill_list(&registry)?,
                SkillCmd::Install { path } => cmd_skill_install(&registry, &path)?,
                SkillCmd::Update { name, path } => cmd_skill_update(&registry, &name, &path)?,
                SkillCmd::Remove { name } => cmd_skill_remove(&registry, &name)?,
                SkillCmd::Sync { path } => cmd_skill_sync(&registry, &path)?,
                SkillCmd::LinkIdes { dir } => {
                    cmd_skill_link_ides(&SkillRegistry::default_path(), &dir)?;
                }
            }
        }
        Cmd::Task { action } => dispatch_task(action, &sock).await?,
        Cmd::Session { action } => dispatch_session(action, &sock).await?,
        Cmd::Cost { session, json, .. } => dispatch_cost(session, json, &sock).await?,
        Cmd::Metrics {
            tier,
            since,
            until,
            runner,
            json,
        } => dispatch_metrics(tier, since, until, runner, json, &sock).await?,
        Cmd::Savings {
            tier,
            since,
            until,
            json,
        } => dispatch_savings(tier, since, until, json, &sock).await?,
        Cmd::Workspace { action } => dispatch_workspace(action, &sock).await?,
        Cmd::Audit { action } => dispatch_audit(action, &sock).await?,
        Cmd::Loop { action } => dispatch_loop(action, &sock).await?,
        Cmd::Sandbox { action } => match action {
            SandboxCmd::Build => {
                println!("Building smedja-sandbox:latest...");
                let status = std::process::Command::new("docker")
                    .args(["build", "-t", "smedja-sandbox:latest", "scripts/sandbox/"])
                    .status()
                    .map_err(|e| anyhow::anyhow!("docker not found: {e}"))?;
                if status.success() {
                    println!("Image built successfully.");
                } else {
                    anyhow::bail!("docker build failed");
                }
            }
            SandboxCmd::Status => {
                let status = SandboxStatus::detect();
                println!("Sandbox backend: {}", status.backend);
                println!(
                    "Available:       {}",
                    if status.available { "yes" } else { "no" }
                );
                println!("Network policy:  {}", status.network_policy);
                println!("Fallback mode:   {}", status.mode);
            }
        },
        Cmd::Mcp { action } => {
            let mut client = Client::connect(&sock)
                .await
                .with_context(|| format!("smdjad not running ({})", sock.display()))?;
            match action {
                McpCmd::Add { name, url, stdio } => {
                    let resp = client
                        .call(
                            "mcp.register",
                            json!({
                                "name": name,
                                "url": url,
                                "transport": if stdio.is_some() { "stdio" } else { "http" },
                                "tools_json": null,
                            }),
                        )
                        .await
                        .context("mcp.register failed")?;
                    println!("registered: {}", resp["name"].as_str().unwrap_or(&name));
                }
                McpCmd::List => {
                    let servers = client
                        .call("mcp.list", json!({}))
                        .await
                        .context("mcp.list failed")?;
                    if let Some(arr) = servers.as_array() {
                        for s in arr {
                            println!(
                                "{} {} ({})",
                                s["name"].as_str().unwrap_or("?"),
                                s["url"].as_str().unwrap_or(""),
                                s["transport"].as_str().unwrap_or("?"),
                            );
                        }
                    }
                }
                McpCmd::Remove { name } => {
                    client
                        .call("mcp.remove", json!({ "name": name }))
                        .await
                        .context("mcp.remove failed")?;
                    println!("removed: {name}");
                }
                McpCmd::Refresh { name } => {
                    let mut params = serde_json::json!({});
                    if let Some(n) = name {
                        params["name"] = serde_json::Value::String(n);
                    }
                    let result: serde_json::Value = client
                        .call("mcp.refresh", params)
                        .await
                        .map_err(|e| anyhow::anyhow!("mcp.refresh failed: {e}"))?;
                    println!(
                        "Refreshed {} server(s)",
                        result
                            .get("refreshed")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0)
                    );
                }
            }
        }
        Cmd::Prices { action } => match action {
            PricesCmd::Update { file } => {
                if let Some(src) = file {
                    // ponytail: copy file to daemon config dir; daemon reloads on next request
                    let dest = xdg_config_dir().join("smedja").join("prices.toml");
                    std::fs::copy(&src, &dest)?;
                    println!("prices.toml updated \u{2192} {}", dest.display());
                } else {
                    // Print the embedded prices.toml location
                    println!("prices.toml is read from the daemon's config directory at startup");
                }
            }
        },
        Cmd::Timeline { action } => dispatch_timeline(action)?,
        Cmd::Service { action } => service::run(&action)?,
        Cmd::Security { action } => match action {
            SecurityCmd::Scan { path } => {
                let target = path.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
                cmd_security_scan(&target);
            }
            SecurityCmd::Report => {
                let db_path = default_ingot_path();
                let ingot = Ingot::open(&db_path)
                    .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
                cmd_security_report(&ingot)?;
            }
            SecurityCmd::Sbom { lockfile } => {
                let lock = lockfile.unwrap_or_else(|| PathBuf::from("Cargo.lock"));
                cmd_security_sbom(&lock)?;
            }
        },
        Cmd::Term { action } => match action {
            TermCmd::Install { bin_path, prefix } => {
                let prefix = prefix.unwrap_or_else(|| {
                    std::env::var("HOME").map_or_else(
                        |_| PathBuf::from(".local/bin"),
                        |h| PathBuf::from(h).join(".local/bin"),
                    )
                });
                let url = bin_path.unwrap_or_else(|| {
                    let os = std::env::consts::OS;
                    if os == "macos" {
                        "https://github.com/mwigge/smedja/releases/latest/download/smedja-darwin-x86_64.tar.gz".to_owned()
                    } else {
                        "https://github.com/mwigge/smedja/releases/latest/download/smedja-linux-x86_64.tar.gz".to_owned()
                    }
                });
                let prefix_clone = prefix.clone();
                let url_clone = url.clone();
                tokio::task::spawn_blocking(move || cmd_term_install(&url_clone, &prefix_clone))
                    .await
                    .context("install task panicked")??;
            }
            TermCmd::ConvertWezterm => {
                eprintln!("smj term convert-wezterm: not yet implemented");
                std::process::exit(1);
            }
        },
        Cmd::Eval { action } => match action {
            EvalCmd::Run {
                suite,
                online,
                json,
                threshold,
            } => cmd_eval_run(&suite, online, json, threshold)?,
        },
        Cmd::Gov { action } => dispatch_gov(action)?,
        Cmd::Doctor { json } => cmd_doctor(&sock, json).await?,
        Cmd::ToolGate => cmd_tool_gate(&sock).await,
        Cmd::Local { action } => dispatch_local(action, &sock).await?,
    }
    Ok(())
}
