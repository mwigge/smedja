use super::*;
use crate::audit::dispatch_audit;
use crate::daemon::{dispatch_daemon, init_tracing};
use crate::eval::dispatch_eval;
use crate::governance::dispatch_gov;
use crate::local::dispatch_local;
use crate::loop_cmd::dispatch_loop;
use crate::mcp::dispatch_mcp;
use crate::prices::dispatch_prices;
use crate::sandbox::dispatch_sandbox;
use crate::security::dispatch_security;
use crate::sessions::dispatch_session;
use crate::skills::dispatch_skill;
use crate::tasks::dispatch_task;
use crate::terminal::dispatch_term;
use crate::timeline::dispatch_timeline;
use crate::usage::{dispatch_cost, dispatch_metrics, dispatch_savings};
use crate::workspace::dispatch_workspace;

pub async fn run() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();
    let sock = cli.sock.unwrap_or_else(default_socket_path);

    match cli.command {
        Cmd::Daemon { action } => dispatch_daemon(action, &sock).await?,
        Cmd::Skill { action } => dispatch_skill(action)?,
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
        Cmd::Sandbox { action } => dispatch_sandbox(action)?,
        Cmd::Mcp { action } => dispatch_mcp(action, &sock).await?,
        Cmd::Prices { action } => dispatch_prices(action)?,
        Cmd::Timeline { action } => dispatch_timeline(action)?,
        Cmd::Service { action } => service::run(&action)?,
        Cmd::Security { action } => dispatch_security(action)?,
        Cmd::Term { action } => dispatch_term(action).await?,
        Cmd::Eval { action } => dispatch_eval(action)?,
        Cmd::Gov { action } => dispatch_gov(action)?,
        Cmd::Doctor { json } => cmd_doctor(&sock, json).await?,
        Cmd::ToolGate => cmd_tool_gate(&sock).await,
        Cmd::Local { action } => dispatch_local(action, &sock).await?,
    }
    Ok(())
}
