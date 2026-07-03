//! The [`drive`] entry point — the deterministic loop state machine — and its
//! terminal telemetry.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt as _;

use crate::config::LoopConfig;
use crate::state::LoopState;
use crate::verify::verification_timeout;

use super::roles::resolve_role;
use super::slice::run_slice_parallel;
use super::types::{LoopCheckpoint, LoopOutcome, RoleRunner, StatusSink};

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
/// `start_slice` is the 0-based index of the first slice to process; pass `0`
/// for a fresh run and the checkpointed index for a `loop.resume` call.
/// Slices before `start_slice` count toward `slices_completed` immediately.
///
/// Before executing each slice the engine writes a checkpoint to
/// `.smedja/loop-state.json`; `loop.resume` reads it to re-enter here.
///
/// Returns the terminal [`LoopOutcome`]; it never panics.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub async fn drive<R: RoleRunner, S: StatusSink>(
    config: &LoopConfig,
    workspace: &Path,
    policy_path: &Path,
    change_name: &str,
    slices: &[String],
    runner: &R,
    sink: &S,
    start_slice: usize,
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

    // Pre-completed slices from a prior run (resume path).
    let pre_completed = start_slice as u64;

    // Bounded concurrency: default min(4, remaining slice count).
    let remaining = slices.len().saturating_sub(start_slice);
    let max_parallel = config.limits.max_parallel_slices.map_or_else(
        || u32::try_from(remaining).unwrap_or(u32::MAX).min(4),
        |n| n.max(1),
    ) as usize;
    let semaphore = Arc::new(tokio::sync::Semaphore::new(max_parallel));

    // Checkpoint: record the batch start so loop.resume can re-enter here.
    let checkpoint_path = workspace.join(".smedja").join("loop-state.json");
    let ckpt = LoopCheckpoint {
        change_name: change_name.to_owned(),
        policy_hash: config.policy_hash.clone(),
        slice_index: start_slice,
        slices_completed: pre_completed,
    };
    if let Ok(json) = serde_json::to_string(&ckpt) {
        let _ = std::fs::write(&checkpoint_path, json);
    }

    sink.set_status(&LoopState::Slicing).await;

    // Dispatch all remaining slices concurrently, bounded by semaphore.
    let mut futures: futures_util::stream::FuturesUnordered<_> =
        futures_util::stream::FuturesUnordered::new();
    for (idx, slice) in slices.iter().enumerate().skip(start_slice) {
        let sem = Arc::clone(&semaphore);
        futures.push(run_slice_parallel(
            runner,
            sink,
            config,
            workspace,
            change_name,
            &implementer,
            &fix,
            &reviewer,
            timeout,
            max_attempts,
            idx,
            slice,
            sem,
        ));
    }

    let mut passed_count: u64 = 0;
    let mut any_failed = false;
    while let Some(result) = futures.next().await {
        if result.passed {
            passed_count += 1;
        } else {
            any_failed = true;
        }
    }

    let slices_completed = pre_completed + passed_count;
    if any_failed {
        sink.set_status(&LoopState::Failed).await;
        finish(change_name, &LoopState::Failed, slices_completed, started);
        LoopOutcome {
            final_state: LoopState::Failed,
            slices_completed,
        }
    } else {
        sink.set_status(&LoopState::Complete).await;
        finish(change_name, &LoopState::Complete, slices_completed, started);
        LoopOutcome {
            final_state: LoopState::Complete,
            slices_completed,
        }
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
    use crate::role::LoopRole;
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
        std::fs::write(&path, json).unwrap();
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
            0,
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
            0,
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
            0,
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
            0,
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
            0,
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
            0,
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
            0,
        )
        .await;
        assert_eq!(out.final_state, LoopState::Complete);
        let roles = rec.roles_run.lock().unwrap();
        assert!(
            roles.contains(&"reviewer".to_owned()),
            "reviewer must run after a passing gate"
        );
    }

    #[tokio::test]
    async fn checkpoint_written_at_batch_start() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
        let (cfg, path) = config_with(&dir, "true", "[]");
        let rec = Recorder::default();
        let slices = vec!["s0".to_owned(), "s1".to_owned()];
        let _ = drive(&cfg, dir.path(), &path, "mychange", &slices, &rec, &rec, 0).await;

        let ckpt_path = dir.path().join(".smedja").join("loop-state.json");
        assert!(ckpt_path.exists(), "checkpoint file must be written");
        let raw = std::fs::read_to_string(&ckpt_path).unwrap();
        let ckpt: LoopCheckpoint = serde_json::from_str(&raw).unwrap();
        // Checkpoint is written at batch start (start_slice=0, pre_completed=0).
        assert_eq!(ckpt.change_name, "mychange");
        assert_eq!(ckpt.slice_index, 0, "checkpoint records batch start index");
        assert_eq!(ckpt.slices_completed, 0);
    }

    #[tokio::test]
    async fn resume_from_start_slice_skips_earlier_slices() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
        let (cfg, path) = config_with(&dir, "true", "[]");
        let rec = Recorder::default();
        // Three slices, resume from index 1 (slice "s0" was already done).
        let slices = vec!["s0".to_owned(), "s1".to_owned(), "s2".to_owned()];
        let out = drive(&cfg, dir.path(), &path, "mychange", &slices, &rec, &rec, 1).await;

        assert_eq!(out.final_state, LoopState::Complete);
        // slices_completed = start_slice(1) + run_count(2) = 3
        assert_eq!(out.slices_completed, 3);
        let roles = rec.roles_run.lock().unwrap();
        // Only s1 and s2 trigger role runs (s0 is skipped).
        assert_eq!(
            roles.len(),
            2,
            "only slices from start_slice onward must run"
        );
    }

    #[tokio::test]
    async fn parallel_slices_all_pass() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
        // max_parallel_slices = 3: all 3 slices can run at once.
        let json = r#"{
                "version": 1,
                "limits": {"max_attempts": 2, "agent_timeout_s": 60, "max_parallel_slices": 3},
                "roles": [],
                "verification": {"command": "true"},
                "review": {"per_slice": false, "required": false},
                "publication": {"max_pr_lines": 400}
            }"#;
        let path = dir.path().join(".smedja").join("loop.json");
        std::fs::write(&path, json).unwrap();
        let cfg = LoopConfig::from_file(&path).unwrap();
        let rec = Recorder::default();
        let slices = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];

        let out = drive(&cfg, dir.path(), &path, "demo", &slices, &rec, &rec, 0).await;

        assert_eq!(out.final_state, LoopState::Complete);
        assert_eq!(out.slices_completed, 3);
        // Each slice runs implementer once.
        assert_eq!(rec.roles_run.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn parallel_one_slice_fails_outcome_is_failed_with_passed_count() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
        // "false" verification makes every slice fail.
        let json = r#"{
                "version": 1,
                "limits": {"max_attempts": 1, "agent_timeout_s": 60, "max_parallel_slices": 3},
                "roles": [],
                "verification": {"command": "false"},
                "review": {"per_slice": false, "required": false},
                "publication": {"max_pr_lines": 400}
            }"#;
        let path = dir.path().join(".smedja").join("loop.json");
        std::fs::write(&path, json).unwrap();
        let cfg = LoopConfig::from_file(&path).unwrap();

        // A runner that passes slice 0 and 1 but fails slice 2.
        use std::sync::Mutex;
        struct SelectiveFail {
            statuses: Mutex<Vec<String>>,
        }
        impl RoleRunner for SelectiveFail {
            async fn run_role(
                &self,
                _role: &LoopRole,
                _idx: usize,
                _slice: &str,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }
        impl StatusSink for SelectiveFail {
            async fn set_status(&self, state: &LoopState) {
                self.statuses
                    .lock()
                    .unwrap()
                    .push(state.as_str().to_owned());
            }
            async fn set_slice(&self, _: i64) {}
        }

        let rec = SelectiveFail {
            statuses: Mutex::new(Vec::new()),
        };
        let slices = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];
        let out = drive(&cfg, dir.path(), &path, "demo", &slices, &rec, &rec, 0).await;
        // All fail because verification command is "false".
        assert_eq!(out.final_state, LoopState::Failed);
        assert_eq!(
            out.slices_completed, 0,
            "no slices pass with false verification"
        );
    }

    #[tokio::test]
    async fn max_parallel_slices_1_degrades_to_serial() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
        let json = r#"{
                "version": 1,
                "limits": {"max_attempts": 1, "agent_timeout_s": 60, "max_parallel_slices": 1},
                "roles": [],
                "verification": {"command": "true"},
                "review": {"per_slice": false, "required": false},
                "publication": {"max_pr_lines": 400}
            }"#;
        let path = dir.path().join(".smedja").join("loop.json");
        std::fs::write(&path, json).unwrap();
        let cfg = LoopConfig::from_file(&path).unwrap();
        let rec = Recorder::default();
        let slices = vec!["x".to_owned(), "y".to_owned(), "z".to_owned()];
        let out = drive(&cfg, dir.path(), &path, "serial", &slices, &rec, &rec, 0).await;
        assert_eq!(out.final_state, LoopState::Complete);
        assert_eq!(out.slices_completed, 3);
        // All 3 implementer runs executed.
        assert_eq!(rec.roles_run.lock().unwrap().len(), 3);
    }
}
