//! Engine — drives the bounded multi-role loop pipeline.
//!
//! The engine owns the deterministic control flow (state machine, retry bound,
//! verification gate, policy/evaluator integrity checks, failure mining) and
//! delegates the side-effecting work — running a role's agent session and
//! persisting loop status — to caller-supplied implementations of [`RoleRunner`]
//! and [`StatusSink`]. This keeps the daemon's provider/session/DB coupling out
//! of the engine crate and makes the pipeline unit-testable with fakes.

use std::future::Future;
use std::path::Path;
use std::time::Instant;

use opentelemetry::trace::{Span as _, Tracer as _};

use crate::config::LoopConfig;
use crate::role::{DataAccess, LoopRole, Runner, Tier};
use crate::state::LoopState;
use crate::verify::{run_verification, verification_timeout};

/// Executes a single loop role — the daemon spawns an agent session for it.
pub trait RoleRunner {
    /// Runs `role` for the slice at `slice_index` with text `slice`.
    ///
    /// Returns `Ok(())` when the role's session completed and `Err` when it
    /// could not be executed. A review verdict is modelled as execution success;
    /// the deterministic pass/fail signal comes from the verification gate.
    fn run_role(
        &self,
        role: &LoopRole,
        slice_index: usize,
        slice: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// Persists loop progression — status transitions and the slice counter.
pub trait StatusSink {
    /// Records the current [`LoopState`].
    fn set_status(&self, state: &LoopState) -> impl Future<Output = ()> + Send;
    /// Records the current 1-based slice counter.
    fn set_slice(&self, slice: i64) -> impl Future<Output = ()> + Send;
}

/// Outcome of driving a loop to a terminal state.
#[derive(Debug, Clone)]
pub struct LoopOutcome {
    /// The terminal [`LoopState`] reached.
    pub final_state: LoopState,
    /// Number of slices that passed verification.
    pub slices_completed: u64,
}

/// Resolves a role by name from the config, falling back to the default table,
/// then to a deny-all local role.
fn resolve_role(config: &LoopConfig, name: &str) -> LoopRole {
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
async fn run_role_traced<R: RoleRunner>(
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

/// Drives the bounded loop pipeline over `slices` to a terminal state.
///
/// Integrity checks run first, then the per-slice pipeline:
/// 1. Re-verify the on-disk policy hash; a mismatch yields
///    [`LoopState::PolicyTampered`].
/// 2. Enforce evaluator separation; a violation yields [`LoopState::Failed`].
/// 3. For each slice: run the implementer, run the deterministic verification
///    gate, and on failure run the fix role and retry up to
///    `limits.max_attempts`. When `review.per_slice` is set the reviewer runs
///    after a passing gate.
///
/// Returns the terminal [`LoopOutcome`]; it never panics.
pub async fn drive<R: RoleRunner, S: StatusSink>(
    config: &LoopConfig,
    workspace: &Path,
    policy_path: &Path,
    change_name: &str,
    slices: &[String],
    runner: &R,
    sink: &S,
) -> LoopOutcome {
    let started = Instant::now();

    // 1. Policy-hash tamper detection — fail closed.
    if config.verify_policy(policy_path).is_err() {
        sink.set_status(&LoopState::PolicyTampered).await;
        finish(change_name, &LoopState::PolicyTampered, 0, started);
        return LoopOutcome {
            final_state: LoopState::PolicyTampered,
            slices_completed: 0,
        };
    }

    // 2. Evaluator/generator separation — fail closed.
    if !config.evaluator_separation_satisfied() {
        let _ = crate::mining::write_failure_guide(
            "reviewer",
            &["reviewer and implementer must use different runners".to_owned()],
            workspace,
        );
        sink.set_status(&LoopState::Failed).await;
        finish(change_name, &LoopState::Failed, 0, started);
        return LoopOutcome {
            final_state: LoopState::Failed,
            slices_completed: 0,
        };
    }

    sink.set_status(&LoopState::Planning).await;

    let implementer = resolve_role(config, "implementer");
    let fix = resolve_role(config, "fix");
    let reviewer = resolve_role(config, "reviewer");
    let timeout = verification_timeout();
    let max_attempts = config.limits.max_attempts.max(1);

    let mut completed: u64 = 0;
    for (idx, slice) in slices.iter().enumerate() {
        sink.set_status(&LoopState::Slicing).await;
        #[allow(clippy::cast_possible_wrap)] // slice index never exceeds i64::MAX
        sink.set_slice((idx + 1) as i64).await;

        let mut passed = false;
        for attempt in 0..max_attempts {
            // First attempt implements; retries run the fix role.
            let role = if attempt == 0 { &implementer } else { &fix };
            if run_role_traced(runner, role, idx, slice, attempt)
                .await
                .is_err()
            {
                break; // role could not execute; abandon this slice
            }

            sink.set_status(&LoopState::Verifying).await;
            let verified = if config.verification.command.trim().is_empty() {
                true
            } else {
                run_verification(&config.verification.command, timeout)
                    .await
                    .is_ok_and(|r| r.passed())
            };

            if verified {
                if config.review.per_slice {
                    sink.set_status(&LoopState::Reviewing).await;
                    let review_ok = run_role_traced(runner, &reviewer, idx, slice, attempt)
                        .await
                        .is_ok();
                    if config.review.required && !review_ok {
                        let _ = crate::mining::write_failure_guide(
                            "reviewer",
                            &[format!("slice {} rejected by reviewer", idx + 1)],
                            workspace,
                        );
                        sink.set_status(&LoopState::Failed).await;
                        finish(change_name, &LoopState::Failed, completed, started);
                        return LoopOutcome {
                            final_state: LoopState::Failed,
                            slices_completed: completed,
                        };
                    }
                }
                passed = true;
                break;
            }
            sink.set_status(&LoopState::Fixed).await;
        }

        if !passed {
            let _ = crate::mining::write_failure_guide(
                &fix.name,
                &[format!(
                    "slice {} failed verification after {max_attempts} attempt(s)",
                    idx + 1
                )],
                workspace,
            );
            sink.set_status(&LoopState::Failed).await;
            finish(change_name, &LoopState::Failed, completed, started);
            return LoopOutcome {
                final_state: LoopState::Failed,
                slices_completed: completed,
            };
        }
        completed += 1;
    }

    sink.set_status(&LoopState::Complete).await;
    finish(change_name, &LoopState::Complete, completed, started);
    LoopOutcome {
        final_state: LoopState::Complete,
        slices_completed: completed,
    }
}

/// Emits terminal telemetry — slice counter and run duration — for the loop.
fn finish(change_name: &str, state: &LoopState, slices: u64, started: Instant) {
    crate::telemetry::record_loop_metrics(change_name, state.as_str(), slices, 0, 0);
    crate::telemetry::record_loop_duration(
        change_name,
        state.as_str(),
        started.elapsed().as_secs_f64(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    /// Records every role execution and status transition for assertions.
    #[derive(Default)]
    struct Recorder {
        roles_run: Mutex<Vec<String>>,
        statuses: Mutex<Vec<String>>,
    }

    impl RoleRunner for Recorder {
        async fn run_role(
            &self,
            role: &LoopRole,
            _slice_index: usize,
            _slice: &str,
        ) -> anyhow::Result<()> {
            self.roles_run.lock().unwrap().push(role.name.clone());
            Ok(())
        }
    }

    impl StatusSink for Recorder {
        async fn set_status(&self, state: &LoopState) {
            self.statuses
                .lock()
                .unwrap()
                .push(state.as_str().to_owned());
        }
        async fn set_slice(&self, _slice: i64) {}
    }

    /// Writes a `loop.json` with the given verification command and roles, then
    /// loads it so `policy_hash` is populated.
    fn config_with(
        dir: &TempDir,
        command: &str,
        roles_json: &str,
    ) -> (LoopConfig, std::path::PathBuf) {
        let json = format!(
            r#"{{
                "version": 1,
                "limits": {{"max_attempts": 3, "agent_timeout_s": 60}},
                "roles": {roles_json},
                "verification": {{"command": "{command}"}},
                "review": {{"per_slice": false, "required": false}},
                "publication": {{"max_pr_lines": 400}}
            }}"#
        );
        let path = dir.path().join("loop.json");
        std::fs::write(&path, &json).unwrap();
        (LoopConfig::from_file(&path).unwrap(), path)
    }

    #[tokio::test]
    async fn happy_path_completes_with_passing_verification() {
        let dir = TempDir::new().unwrap();
        let (cfg, path) = config_with(&dir, "true", "[]");
        let rec = Recorder::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice one".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Complete);
        assert_eq!(out.slices_completed, 1);
        // Implementer ran exactly once; no fix retries needed.
        assert_eq!(
            *rec.roles_run.lock().unwrap(),
            vec!["implementer".to_owned()]
        );
    }

    #[tokio::test]
    async fn failing_verification_exhausts_retries_then_fails() {
        let dir = TempDir::new().unwrap();
        let (cfg, path) = config_with(&dir, "false", "[]");
        let rec = Recorder::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Failed);
        assert_eq!(out.slices_completed, 0);
        // 3 attempts: implementer, fix, fix.
        let roles = rec.roles_run.lock().unwrap();
        assert_eq!(roles.len(), 3, "max_attempts role executions expected");
        assert_eq!(roles[0], "implementer");
        assert_eq!(roles[1], "fix");
        // A failure guide must have been written.
        assert!(dir
            .path()
            .join(".smedja")
            .join("guides")
            .join("fix.md")
            .exists());
    }

    /// A role runner whose execution always fails (models a role that could not
    /// run, e.g. a per-role timeout firing).
    #[derive(Default)]
    struct FailingRunner {
        statuses: Mutex<Vec<String>>,
    }
    impl RoleRunner for FailingRunner {
        async fn run_role(
            &self,
            _role: &LoopRole,
            _slice_index: usize,
            _slice: &str,
        ) -> anyhow::Result<()> {
            anyhow::bail!("role timed out")
        }
    }
    impl StatusSink for FailingRunner {
        async fn set_status(&self, state: &LoopState) {
            self.statuses
                .lock()
                .unwrap()
                .push(state.as_str().to_owned());
        }
        async fn set_slice(&self, _slice: i64) {}
    }

    #[tokio::test]
    async fn role_execution_failure_fails_the_slice() {
        let dir = TempDir::new().unwrap();
        let (cfg, path) = config_with(&dir, "true", "[]");
        let rec = FailingRunner::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Failed);
        assert_eq!(out.slices_completed, 0);
        // The loop must reach the terminal Failed state.
        assert!(rec.statuses.lock().unwrap().contains(&"failed".to_owned()));
    }

    #[tokio::test]
    async fn evaluator_separation_violation_fails_before_any_role_runs() {
        let dir = TempDir::new().unwrap();
        // reviewer and implementer share runner "local" → separation violated.
        let roles = r#"[
            {"name":"implementer","runner":"local","tier":"local","model":null,"read_only":false,"tools":[]},
            {"name":"reviewer","runner":"local","tier":"fast","model":null,"read_only":true,"tools":[]}
        ]"#;
        let (cfg, path) = config_with(&dir, "true", roles);
        let rec = Recorder::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Failed);
        assert!(
            rec.roles_run.lock().unwrap().is_empty(),
            "no role may run on separation failure"
        );
    }

    #[tokio::test]
    async fn tampered_policy_aborts_with_policy_tampered() {
        let dir = TempDir::new().unwrap();
        let (cfg, path) = config_with(&dir, "true", "[]");
        // Tamper the file on disk after load.
        std::fs::write(&path, r#"{"version":99}"#).unwrap();
        let rec = Recorder::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::PolicyTampered);
        assert!(rec.roles_run.lock().unwrap().is_empty());
        assert!(rec
            .statuses
            .lock()
            .unwrap()
            .contains(&"policy_tampered".to_owned()));
    }

    #[tokio::test]
    async fn review_required_blocks_on_reviewer_failure() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "version": 1,
            "limits": {"max_attempts": 3, "agent_timeout_s": 60},
            "roles": [
                {"name":"implementer","runner":"local","tier":"local","model":null,"read_only":false,"tools":[]},
                {"name":"reviewer","runner":"minimax","tier":"fast","model":null,"read_only":true,"tools":[]}
            ],
            "verification": {"command": "true"},
            "review": {"per_slice": true, "required": true},
            "publication": {"max_pr_lines": 400}
        }"#;
        let path = dir.path().join("loop.json");
        std::fs::write(&path, json).unwrap();
        let cfg = LoopConfig::from_file(&path).unwrap();

        /// Succeeds for every role except "reviewer".
        #[derive(Default)]
        struct ReviewerFailRunner {
            statuses: Mutex<Vec<String>>,
        }
        impl RoleRunner for ReviewerFailRunner {
            async fn run_role(
                &self,
                role: &LoopRole,
                _slice_index: usize,
                _slice: &str,
            ) -> anyhow::Result<()> {
                if role.name == "reviewer" {
                    anyhow::bail!("reviewer rejected the slice")
                }
                Ok(())
            }
        }
        impl StatusSink for ReviewerFailRunner {
            async fn set_status(&self, state: &LoopState) {
                self.statuses
                    .lock()
                    .unwrap()
                    .push(state.as_str().to_owned());
            }
            async fn set_slice(&self, _slice: i64) {}
        }

        let rec = ReviewerFailRunner::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Failed);
        assert_eq!(out.slices_completed, 0);
        assert!(
            rec.statuses.lock().unwrap().contains(&"failed".to_owned()),
            "loop must reach Failed when review.required and reviewer fails"
        );
    }

    #[tokio::test]
    async fn reviewer_runs_after_pass_when_per_slice_review_enabled() {
        let dir = TempDir::new().unwrap();
        let json = r#"{
            "version": 1,
            "limits": {"max_attempts": 3, "agent_timeout_s": 60},
            "roles": [
                {"name":"implementer","runner":"local","tier":"local","model":null,"read_only":false,"tools":[]},
                {"name":"reviewer","runner":"minimax","tier":"fast","model":null,"read_only":true,"tools":[]}
            ],
            "verification": {"command": "true"},
            "review": {"per_slice": true, "required": false},
            "publication": {"max_pr_lines": 400}
        }"#;
        let path = dir.path().join("loop.json");
        std::fs::write(&path, json).unwrap();
        let cfg = LoopConfig::from_file(&path).unwrap();
        let rec = Recorder::default();
        let out = drive(
            &cfg,
            dir.path(),
            &path,
            "demo",
            &["slice".to_owned()],
            &rec,
            &rec,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Complete);
        let roles = rec.roles_run.lock().unwrap();
        assert!(
            roles.contains(&"reviewer".to_owned()),
            "reviewer must run after a passing gate"
        );
    }
}
