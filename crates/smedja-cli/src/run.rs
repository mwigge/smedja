use super::*;
use crate::audit::dispatch_audit;
use crate::governance::dispatch_gov;
use crate::local::dispatch_local;
use crate::loop_cmd::dispatch_loop;
use crate::sessions::dispatch_session;
use crate::tasks::dispatch_task;
use crate::usage::{dispatch_cost, dispatch_metrics, dispatch_savings};

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
        Cmd::Workspace { action } => match action {
            WorkspaceCmd::Agents {
                action: agents_action,
            } => match agents_action {
                AgentsCmd::Show => {
                    let mut client = Client::connect(&sock)
                        .await
                        .with_context(|| format!("smdjad not running ({})", sock.display()))?;
                    cmd_workspace_agents(&mut client).await?;
                }
                AgentsCmd::Init => {
                    let target = std::path::Path::new(".smedja");
                    std::fs::create_dir_all(target)?;
                    let agents_toml = target.join("agents.toml");
                    if agents_toml.exists() {
                        anyhow::bail!(".smedja/agents.toml already exists");
                    }
                    std::fs::write(&agents_toml, AGENTS_TOML_TEMPLATE)?;
                    println!("Created {}", agents_toml.display());
                }
            },
            WorkspaceCmd::Init { path } => {
                let target = path.unwrap_or_else(|| std::env::current_dir().expect("cwd"));
                if !target.exists() {
                    anyhow::bail!("path does not exist: {}", target.display());
                }
                let mut client = Client::connect(&sock)
                    .await
                    .with_context(|| format!("smdjad not running ({})", sock.display()))?;
                let resp = client
                    .call(
                        "graph.index",
                        json!({ "workspace": target.display().to_string() }),
                    )
                    .await
                    .context("graph.index failed")?;
                let count = resp["indexed"].as_u64().unwrap_or(0);
                println!("Indexed {count} symbols in {}", target.display());
                cmd_workspace_init(&target)?;
            }
            WorkspaceCmd::Index { commit_sha } => {
                // `commit_sha` is accepted for CLI compatibility; the server-side
                // index performs a full re-index.
                let _ = commit_sha;
                let workspace =
                    std::env::current_dir().context("cannot determine current directory")?;
                let mut client = Client::connect(&sock)
                    .await
                    .with_context(|| format!("smdjad not running ({})", sock.display()))?;
                let resp = client
                    .call(
                        "graph.index",
                        json!({ "workspace": workspace.display().to_string() }),
                    )
                    .await
                    .context("graph.index failed")?;
                let count = resp["indexed"].as_u64().unwrap_or(0);
                println!("Indexed {count} symbols in {}", workspace.display());
            }
            WorkspaceCmd::Add { path } => {
                let workspace =
                    std::env::current_dir().context("cannot determine current directory")?;
                let smedja_dir = workspace.join(".smedja");
                std::fs::create_dir_all(&smedja_dir)?;
                let toml_path = smedja_dir.join("workspace.toml");

                // Read existing content or start fresh.
                let mut content = if toml_path.exists() {
                    std::fs::read_to_string(&toml_path).context("failed to read workspace.toml")?
                } else {
                    String::new()
                };

                // Append the new path entry.
                if !content.is_empty() && !content.ends_with('\n') {
                    content.push('\n');
                }
                content.push_str("\n[[workspace.paths]]\npath = \"");
                content.push_str(&path);
                content.push_str("\"\n");
                std::fs::write(&toml_path, &content).context("failed to write workspace.toml")?;
                println!("Added path '{}' to {}", path, toml_path.display());
            }
        },
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
        Cmd::Timeline { action } => {
            let db_path = default_ingot_path();
            let ingot = Ingot::open(&db_path)
                .with_context(|| format!("failed to open ingot at {}", db_path.display()))?;
            match action {
                TimelineCmd::Conversations { since, json } => {
                    let rollups = ingot.recent_conversations(50)?;
                    let rollups: Vec<_> = if let Some(since_secs) = since {
                        let cutoff = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs()
                            .saturating_sub(since_secs)
                            .try_into()
                            .unwrap_or(i64::MAX);
                        rollups
                            .into_iter()
                            .filter(|r| r.started_at >= cutoff)
                            .collect()
                    } else {
                        rollups
                    };
                    if json {
                        let arr: Vec<serde_json::Value> = rollups
                            .iter()
                            .map(|r| {
                                serde_json::json!({
                                    "conversation_id": r.conversation_id,
                                    "started_at": r.started_at,
                                    "last_seen_at": r.last_seen_at,
                                    "agent_count": r.agent_count,
                                    "llm_call_count": r.llm_call_count,
                                    "tool_call_count": r.tool_call_count,
                                    "failure_count": r.failure_count,
                                    "input_token_total": r.input_token_total,
                                    "output_token_total": r.output_token_total,
                                })
                            })
                            .collect();
                        println!("{}", serde_json::to_string_pretty(&arr)?);
                    } else if rollups.is_empty() {
                        println!("No conversations found.");
                    } else {
                        println!(
                            "{:<40} {:>8} {:>8} {:>6} {:>6}",
                            "CONVERSATION", "LLM", "TOOLS", "FAIL", "TOKENS"
                        );
                        for r in &rollups {
                            println!(
                                "{:<40} {:>8} {:>8} {:>6} {:>6}",
                                &r.conversation_id[..r.conversation_id.len().min(40)],
                                r.llm_call_count,
                                r.tool_call_count,
                                r.failure_count,
                                r.input_token_total + r.output_token_total,
                            );
                        }
                    }
                }
                TimelineCmd::Show {
                    conversation_id,
                    failures_only,
                    json,
                } => {
                    let events = if failures_only {
                        ingot.failed_events(&conversation_id)?
                    } else {
                        ingot.conversation_timeline(&conversation_id)?
                    };
                    if json {
                        println!("{}", serde_json::to_string_pretty(&events)?);
                    } else if events.is_empty() {
                        println!("No events for conversation {conversation_id}");
                    } else {
                        for ev in &events {
                            println!(
                                "{:.0} {:12} {:8} {:<30} trace:{} span:{}",
                                ev.ts.as_secs_f64(),
                                ev.action_type,
                                ev.status.as_deref().unwrap_or("-"),
                                ev.tool_name.as_deref().unwrap_or(ev.actor.as_str()),
                                ev.trace_id.as_deref().unwrap_or("-"),
                                ev.span_id.as_deref().unwrap_or("-"),
                            );
                        }
                    }
                }
                TimelineCmd::Open { id } => {
                    let template = std::env::var("SMEDJA_TIMELINE_URL").unwrap_or_default();
                    if template.is_empty() {
                        println!("Set SMEDJA_TIMELINE_URL to open traces in a backend.");
                        println!("Example (Honeycomb): SMEDJA_TIMELINE_URL=https://ui.honeycomb.io/your-team/environments/prod/trace?trace_id={{id}}");
                        println!("ID: {id}");
                    } else {
                        let url = template.replace("{id}", &id);
                        println!("{url}");
                    }
                }
            }
        }
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
