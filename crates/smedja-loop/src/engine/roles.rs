//! Role resolution and per-role traced execution.

use opentelemetry::trace::{Span as _, Tracer as _};

use crate::config::LoopConfig;
use crate::role::{DataAccess, LoopRole, Runner, Tier};

use super::types::RoleRunner;

/// Resolves a role by name from the config, falling back to the default table,
/// then to a deny-all local role.
pub(crate) fn resolve_role(config: &LoopConfig, name: &str) -> LoopRole {
    if let Some(r) = config.roles.iter().find(|r| r.name == name) {
        return r.clone();
    }
    LoopRole::defaults()
        .into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| LoopRole {
            name: name.to_owned(),
            runner: Runner::Local,
            tier: Tier::Local,
            model: None,
            read_only: false,
            tools: vec![],
            role_id: uuid::Uuid::nil(),
            data_access: DataAccess::default(),
            resume_session_id: None,
        })
}

/// String label for a runner, for telemetry attributes.
fn runner_label(runner: Runner) -> &'static str {
    match runner {
        Runner::Claude => "claude",
        Runner::Codex => "codex",
        Runner::Local => "local",
        Runner::Copilot => "copilot",
        Runner::Minimax => "minimax",
        Runner::Berget => "berget",
        Runner::Pool => "pool",
    }
}

/// String label for a tier, for telemetry attributes.
fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Fast => "fast",
        Tier::Local => "local",
        Tier::Deep => "deep",
    }
}

/// Runs a role with a per-role telemetry span carrying the standard attributes.
pub(crate) async fn run_role_traced<R: RoleRunner>(
    runner: &R,
    role: &LoopRole,
    slice_index: usize,
    slice: &str,
    attempt: u32,
) -> anyhow::Result<()> {
    let tracer = opentelemetry::global::tracer("smedja.loop");
    let mut span = tracer.start("smedja.loop.role");
    crate::telemetry::set_role_attributes(
        &mut span,
        &role.name,
        runner_label(role.runner),
        tier_label(role.tier),
        attempt,
    );
    let result = runner.run_role(role, slice_index, slice).await;
    if result.is_err() {
        span.set_status(opentelemetry::trace::Status::error("role execution failed"));
    }
    span.end();
    result
}
