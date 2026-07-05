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
fn config_with(dir: &TempDir, command: &str, roles_json: &str) -> (LoopConfig, std::path::PathBuf) {
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
        true,
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
        true,
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
        true,
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
        true,
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
        true,
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
        true,
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
        true,
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
async fn checkpoint_advances_per_completed_slice() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
    let (cfg, path) = config_with(&dir, "true", "[]");
    let rec = Recorder::default();
    let slices = vec!["s0".to_owned(), "s1".to_owned()];
    let _ = drive(
        &cfg,
        dir.path(),
        &path,
        "mychange",
        &slices,
        &rec,
        &rec,
        0,
        true,
    )
    .await;

    let ckpt_path = dir.path().join(".smedja").join("loop-state.json");
    assert!(ckpt_path.exists(), "checkpoint file must be written");
    let raw = std::fs::read_to_string(&ckpt_path).unwrap();
    let ckpt: LoopCheckpoint = serde_json::from_str(&raw).unwrap();
    // After both slices pass, the checkpoint has advanced past the batch so a
    // resume re-enters after the last completed slice instead of at 0.
    assert_eq!(ckpt.change_name, "mychange");
    assert_eq!(
        ckpt.slice_index, 2,
        "checkpoint advances to the next uncompleted slice"
    );
    assert_eq!(ckpt.slices_completed, 2);
}

/// A resume after N completed slices must re-enter at slice N+1, running only
/// the remaining slices — never restarting the whole batch from 0.
#[tokio::test]
async fn resume_after_n_completed_starts_at_n_plus_one() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
    let (cfg, path) = config_with(&dir, "true", "[]");
    let slices = vec!["s0".to_owned(), "s1".to_owned(), "s2".to_owned()];

    // First pass: complete slice 0 only (drive a single-slice batch), which
    // advances the checkpoint to 1.
    let rec1 = Recorder::default();
    let _ = drive(
        &cfg,
        dir.path(),
        &path,
        "mychange",
        &slices[..1],
        &rec1,
        &rec1,
        0,
        true,
    )
    .await;
    let ckpt_path = dir.path().join(".smedja").join("loop-state.json");
    let ckpt: LoopCheckpoint =
        serde_json::from_str(&std::fs::read_to_string(&ckpt_path).unwrap()).unwrap();
    assert_eq!(ckpt.slice_index, 1, "one slice completed → checkpoint at 1");

    // Resume from the checkpoint over the full slice list.
    let rec2 = Recorder::default();
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "mychange",
        &slices,
        &rec2,
        &rec2,
        ckpt.slice_index,
        true,
    )
    .await;
    assert_eq!(out.final_state, LoopState::Complete);
    assert_eq!(out.slices_completed, 3);
    // Only s1 and s2 run on resume — s0 is not re-run.
    assert_eq!(
        *rec2.roles_run.lock().unwrap(),
        vec!["implementer".to_owned(), "implementer".to_owned()],
        "resume runs only slices from the checkpoint onward"
    );
}

/// A shared workspace has no per-slice worktree isolation, so even with
/// `max_parallel_slices > 1` the engine must run slices one at a time.
#[tokio::test]
async fn shared_workspace_forces_serial_execution() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ConcurrencyProbe {
        current: AtomicUsize,
        max_seen: AtomicUsize,
    }
    impl RoleRunner for ConcurrencyProbe {
        async fn run_role(
            &self,
            _role: &LoopRole,
            _idx: usize,
            _slice: &str,
        ) -> anyhow::Result<()> {
            let now = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_seen.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }
    impl StatusSink for ConcurrencyProbe {
        async fn set_status(&self, _state: &LoopState) {}
        async fn set_slice(&self, _slice: i64) {}
    }

    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
    // max_parallel_slices = 3 requests parallelism the shared workspace must veto.
    let json = r#"{
                "version": 1,
                "limits": {"max_attempts": 1, "agent_timeout_s": 60, "max_parallel_slices": 3},
                "roles": [],
                "verification": {"command": "true"},
                "review": {"per_slice": false, "required": false},
                "publication": {"max_pr_lines": 400}
            }"#;
    let path = dir.path().join(".smedja").join("loop.json");
    std::fs::write(&path, json).unwrap();
    let cfg = LoopConfig::from_file(&path).unwrap();
    let slices = vec!["a".to_owned(), "b".to_owned(), "c".to_owned()];

    let probe = ConcurrencyProbe {
        current: AtomicUsize::new(0),
        max_seen: AtomicUsize::new(0),
    };
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &slices,
        &probe,
        &probe,
        0,
        true,
    )
    .await;
    assert_eq!(out.final_state, LoopState::Complete);
    assert_eq!(out.slices_completed, 3);
    assert_eq!(
        probe.max_seen.load(Ordering::SeqCst),
        1,
        "shared workspace must never run two slices concurrently"
    );

    // With an isolated workspace (shared_workspace = false) the same config
    // is allowed to overlap slices.
    let probe2 = ConcurrencyProbe {
        current: AtomicUsize::new(0),
        max_seen: AtomicUsize::new(0),
    };
    let _ = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &slices,
        &probe2,
        &probe2,
        0,
        false,
    )
    .await;
    assert!(
        probe2.max_seen.load(Ordering::SeqCst) > 1,
        "an isolated workspace may run slices concurrently"
    );
}

#[tokio::test]
async fn resume_from_start_slice_skips_earlier_slices() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
    let (cfg, path) = config_with(&dir, "true", "[]");
    let rec = Recorder::default();
    // Three slices, resume from index 1 (slice "s0" was already done).
    let slices = vec!["s0".to_owned(), "s1".to_owned(), "s2".to_owned()];
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "mychange",
        &slices,
        &rec,
        &rec,
        1,
        true,
    )
    .await;

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

    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &slices,
        &rec,
        &rec,
        0,
        true,
    )
    .await;

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
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &slices,
        &rec,
        &rec,
        0,
        true,
    )
    .await;
    // All fail because verification command is "false".
    assert_eq!(out.final_state, LoopState::Failed);
    assert_eq!(
        out.slices_completed, 0,
        "no slices pass with false verification"
    );
}

/// Records role runs as `(name, tier)` pairs plus the plan-phase call, so tests
/// can assert both the plan → implement → verify → review ordering and the
/// per-attempt tier escalation.
#[derive(Default)]
struct PlanRecorder {
    roles_run: Mutex<Vec<(String, crate::role::Tier)>>,
    /// The `existing` slice list the plan phase was handed, or `None` if unrun.
    plan_input: Mutex<Option<Vec<String>>>,
    /// The slice list the plan phase produces (returned from `run_plan`).
    produced: Vec<String>,
}

impl RoleRunner for PlanRecorder {
    async fn run_role(
        &self,
        role: &LoopRole,
        _slice_index: usize,
        _slice: &str,
    ) -> anyhow::Result<()> {
        self.roles_run
            .lock()
            .unwrap()
            .push((role.name.clone(), role.tier));
        Ok(())
    }
    async fn run_plan(&self, _role: &LoopRole, existing: &[String]) -> anyhow::Result<Vec<String>> {
        *self.plan_input.lock().unwrap() = Some(existing.to_vec());
        Ok(self.produced.clone())
    }
}
impl StatusSink for PlanRecorder {
    async fn set_status(&self, _state: &LoopState) {}
    async fn set_slice(&self, _slice: i64) {}
}

#[tokio::test]
async fn plan_phase_runs_first_and_produces_the_slice_list() {
    let dir = TempDir::new().unwrap();
    // A configured `plan` role opts the run into the executed plan phase.
    let roles = r#"[
            {"name":"plan","runner":"claude","tier":"deep","model":null,"read_only":false,"tools":[]},
            {"name":"implementer","runner":"local","tier":"local","model":null,"read_only":false,"tools":[]},
            {"name":"reviewer","runner":"minimax","tier":"fast","model":null,"read_only":true,"tools":[]}
        ]"#;
    let (cfg, path) = config_with(&dir, "true", roles);
    let rec = PlanRecorder {
        produced: vec!["planned-1".to_owned(), "planned-2".to_owned()],
        ..PlanRecorder::default()
    };
    // The engine is handed an EMPTY slice list (no tasks.md yet); the plan phase
    // decomposes the work and hands back two slices for the pipeline to drive.
    let out = drive(&cfg, dir.path(), &path, "demo", &[], &rec, &rec, 0, true).await;

    assert_eq!(out.final_state, LoopState::Complete);
    assert_eq!(
        out.slices_completed, 2,
        "the pipeline drives exactly the slices the plan phase produced"
    );
    assert!(
        rec.plan_input.lock().unwrap().is_some(),
        "the plan phase must run first on a fresh run with a configured plan role"
    );
    // One implementer run per produced slice — plan → implement.
    let names: Vec<String> = rec
        .roles_run
        .lock()
        .unwrap()
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    assert_eq!(
        names,
        vec!["implementer".to_owned(), "implementer".to_owned()]
    );
}

#[tokio::test]
async fn plan_phase_skipped_without_a_configured_plan_role() {
    let dir = TempDir::new().unwrap();
    // No `plan` role → behavior-compatible: slices are handed in unchanged.
    let (cfg, path) = config_with(&dir, "true", "[]");
    let rec = PlanRecorder {
        produced: vec!["ignored".to_owned()],
        ..PlanRecorder::default()
    };
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &["given".to_owned()],
        &rec,
        &rec,
        0,
        true,
    )
    .await;
    assert_eq!(out.final_state, LoopState::Complete);
    assert_eq!(out.slices_completed, 1);
    assert!(
        rec.plan_input.lock().unwrap().is_none(),
        "no plan phase without a configured plan role"
    );
}

#[tokio::test]
async fn plan_phase_skipped_on_resume() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".smedja")).unwrap();
    let roles = r#"[
            {"name":"plan","runner":"claude","tier":"deep","model":null,"read_only":false,"tools":[]},
            {"name":"implementer","runner":"local","tier":"local","model":null,"read_only":false,"tools":[]},
            {"name":"reviewer","runner":"minimax","tier":"fast","model":null,"read_only":true,"tools":[]}
        ]"#;
    let (cfg, path) = config_with(&dir, "true", roles);
    let rec = PlanRecorder {
        produced: vec!["planned".to_owned()],
        ..PlanRecorder::default()
    };
    // start_slice = 1 → this is a resume; the plan phase must NOT re-run.
    let slices = vec!["s0".to_owned(), "s1".to_owned()];
    let _ = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &slices,
        &rec,
        &rec,
        1,
        true,
    )
    .await;
    assert!(
        rec.plan_input.lock().unwrap().is_none(),
        "resume must not re-plan — slices are already on disk"
    );
}

#[tokio::test]
async fn retry_escalates_the_fix_role_tier() {
    use crate::role::Tier;
    let dir = TempDir::new().unwrap();
    // "false" verification fails every attempt; max_attempts = 3 exercises two retries.
    let (cfg, path) = config_with(&dir, "false", "[]");
    let rec = PlanRecorder::default();
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "demo",
        &["slice".to_owned()],
        &rec,
        &rec,
        0,
        true,
    )
    .await;
    assert_eq!(out.final_state, LoopState::Failed);
    let runs = rec.roles_run.lock().unwrap();
    assert_eq!(runs.len(), 3, "implementer + two escalating fix retries");
    // attempt 0: implementer at its base tier (Local by default).
    assert_eq!(runs[0], ("implementer".to_owned(), Tier::Local));
    // attempt 1: fix escalates one rank Local → Fast.
    assert_eq!(runs[1], ("fix".to_owned(), Tier::Fast));
    // attempt 2: fix escalates again Fast → Deep (capped at Deep).
    assert_eq!(runs[2], ("fix".to_owned(), Tier::Deep));
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
    let out = drive(
        &cfg,
        dir.path(),
        &path,
        "serial",
        &slices,
        &rec,
        &rec,
        0,
        true,
    )
    .await;
    assert_eq!(out.final_state, LoopState::Complete);
    assert_eq!(out.slices_completed, 3);
    // All 3 implementer runs executed.
    assert_eq!(rec.roles_run.lock().unwrap().len(), 3);
}
