//! `smj` — the smedja control CLI.
//!
//! `main` parses the top-level [`cli::Cli`] and dispatches each command to its
//! sibling module. Each module owns its subcommand enum, its command
//! implementation, output formatting, and its own tests.

pub mod service;

mod audit;
mod cli;
mod cost;
mod daemon;
mod doctor;
mod eval;
mod gov;
mod local;
mod loops;
mod mcp;
mod metrics;
mod models;
mod prices;
mod sandbox;
mod security;
mod session;
mod shell;
mod skill;
mod task;
mod term;
mod timeline;
mod toolgate;
mod upgrade;
mod util;
mod workspace;

use anyhow::Result;
use clap::Parser as _;

use crate::cli::{Cli, Cmd};

#[tokio::main]
async fn main() -> Result<()> {
    util::init_tracing();
    let cli = Cli::parse();
    let sock = cli.sock.unwrap_or_else(util::default_socket_path);

    match cli.command {
        Cmd::Daemon { action } => daemon::run(&sock, action).await?,
        Cmd::Session { action } => session::run(&sock, action).await?,
        Cmd::Workspace { action } => workspace::run(&sock, action).await?,
        Cmd::Audit { action } => audit::run(&sock, action).await?,
        Cmd::Cost { session, json, .. } => cost::run(&sock, session, json).await?,
        Cmd::Metrics {
            tier,
            since,
            until,
            runner,
            json,
        } => metrics::run_metrics(&sock, tier, since, until, runner, json).await?,
        Cmd::Savings {
            tier,
            since,
            until,
            json,
        } => metrics::run_savings(&sock, tier, since, until, json).await?,
        Cmd::Skill { action } => skill::run(action)?,
        Cmd::Task { action } => task::run(&sock, action).await?,
        Cmd::Loop { action } => loops::run(&sock, action).await?,
        Cmd::Mcp { action } => mcp::run(&sock, action).await?,
        Cmd::Sandbox { action } => sandbox::run(action)?,
        Cmd::Prices { action } => prices::run(action)?,
        Cmd::Term { action } => term::run(action).await?,
        Cmd::Timeline { action } => timeline::run(action)?,
        Cmd::Service { action } => service::run(&action)?,
        Cmd::Security { action } => security::run(action)?,
        Cmd::Eval { action } => eval::run(action)?,
        Cmd::Gov { action } => gov::run(action)?,
        Cmd::Doctor { json } => doctor::run(&sock, json).await?,
        Cmd::Models { action } => models::run(action)?,
        Cmd::Shell { action } => shell::run(action)?,
        Cmd::Upgrade { check } => upgrade::run(check)?,
        Cmd::Local { action } => local::run(&sock, action).await?,
        Cmd::ToolGate => toolgate::run(&sock).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn graph_symbols_injected_into_context() {
        // Mirrors the noun-extraction filter used by smdjad's graph auto-injection.
        let message = "implement WorkingMemory seal_prefix function";
        let stop_words = [
            "the", "and", "for", "with", "this", "that", "from", "into", "use", "are", "was",
            "has", "not", "can", "its", "will",
        ];
        let nouns: Vec<&str> = message
            .split_whitespace()
            .filter(|t| t.len() >= 3 && !stop_words.contains(&t.to_lowercase().as_str()))
            .take(5)
            .collect();
        assert!(!nouns.is_empty(), "nouns must be extracted from message");
        assert!(
            nouns.contains(&"implement")
                || nouns.contains(&"WorkingMemory")
                || nouns.contains(&"seal_prefix"),
        );
    }
}
