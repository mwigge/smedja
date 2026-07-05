//! Engine — drives the bounded multi-role loop pipeline.
//!
//! The engine owns the deterministic control flow (state machine, retry bound,
//! verification gate, policy/evaluator integrity checks, failure mining) and
//! delegates the side-effecting work — running a role's agent session and
//! persisting loop status — to caller-supplied implementations of [`RoleRunner`]
//! and [`StatusSink`]. This keeps the daemon's provider/session/DB coupling out
//! of the engine crate and makes the pipeline unit-testable with fakes.

mod checkpoint;
#[cfg(test)]
mod tests;
mod types;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use futures_util::StreamExt as _;
use opentelemetry::trace::{Span as _, Tracer as _};

use crate::config::LoopConfig;
use crate::role::LoopRole;
use crate::state::LoopState;
use crate::verify::{run_verification, verification_timeout};

use checkpoint::{resolve_role, runner_label, tier_label, write_checkpoint};

pub use types::{LoopCheckpoint, LoopOutcome, RoleRunner, StatusSink};

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
/// `start_slice` is the 0-based index of the first slice to process; pass `0`
/// for a fresh run and the checkpointed index for a `loop.resume` call.
/// Slices before `start_slice` count toward `slices_completed` immediately.
///
/// The engine writes an initial checkpoint to `.smedja/loop-state.json` and then
/// advances it after each slice passes, so `loop.resume` re-enters at the first
/// uncompleted slice rather than re-running the whole batch.
///
/// `shared_workspace` guards against concurrent slices corrupting a single tree:
/// the engine has no per-slice worktree isolation, so when the slices operate on
/// one shared workspace (`true`) execution is forced serial regardless of
/// `max_parallel_slices`. A caller that gives each slice its own worktree passes
/// `false` to honour the configured parallelism.
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
    shared_workspace: bool,
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

    // Bounded concurrency: default min(4, remaining slice count). A shared
    // workspace has no per-slice worktree isolation, so concurrent slices would
    // edit the same tree and run the verification command against each other's
    // half-finished changes — force serial in that case.
    let remaining = slices.len().saturating_sub(start_slice);
    let configured_parallel = config.limits.max_parallel_slices.map_or_else(
        || u32::try_from(remaining).unwrap_or(u32::MAX).min(4),
        |n| n.max(1),
    ) as usize;
    let max_parallel = if shared_workspace {
        1
    } else {
        configured_parallel.max(1)
    };

    // Initial checkpoint: a resume before any slice completes re-enters at
    // `start_slice`. The checkpoint then advances after each slice passes.
    let checkpoint_path = workspace.join(".smedja").join("loop-state.json");
    write_checkpoint(
        &checkpoint_path,
        change_name,
        &config.policy_hash,
        start_slice,
        pre_completed,
    );

    sink.set_status(&LoopState::Slicing).await;

    let mut passed_count: u64 = 0;
    let mut any_failed = false;

    if max_parallel <= 1 {
        // Serial: run slices in order, advancing the checkpoint after each pass
        // so `loop.resume` skips completed slices and re-enters at the first
        // uncompleted one. Stop at the first failure so resume retries it.
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        for (idx, slice) in slices.iter().enumerate().skip(start_slice) {
            let result = run_slice_parallel(
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
                Arc::clone(&semaphore),
            )
            .await;
            if result.passed {
                passed_count += 1;
                write_checkpoint(
                    &checkpoint_path,
                    change_name,
                    &config.policy_hash,
                    idx + 1,
                    pre_completed + passed_count,
                );
            } else {
                any_failed = true;
                break;
            }
        }
    } else {
        // Parallel: each slice owns an isolated worktree. Dispatch all remaining
        // slices concurrently, bounded by the semaphore, then advance the
        // checkpoint over the contiguous run of completed slices.
        let semaphore = Arc::new(tokio::sync::Semaphore::new(max_parallel));
        // `async move` would move the owned role values; bind Copy references so
        // each per-slice future borrows the shared roles instead.
        let (impl_ref, fix_ref, rev_ref) = (&implementer, &fix, &reviewer);
        let mut futures: futures_util::stream::FuturesUnordered<_> =
            futures_util::stream::FuturesUnordered::new();
        for (idx, slice) in slices.iter().enumerate().skip(start_slice) {
            let sem = Arc::clone(&semaphore);
            futures.push(async move {
                let r = run_slice_parallel(
                    runner,
                    sink,
                    config,
                    workspace,
                    change_name,
                    impl_ref,
                    fix_ref,
                    rev_ref,
                    timeout,
                    max_attempts,
                    idx,
                    slice,
                    sem,
                )
                .await;
                (idx, r.passed)
            });
        }

        let mut passed_indices = std::collections::BTreeSet::new();
        while let Some((idx, passed)) = futures.next().await {
            if passed {
                passed_count += 1;
                passed_indices.insert(idx);
            } else {
                any_failed = true;
            }
        }

        let mut next = start_slice;
        while passed_indices.contains(&next) {
            next += 1;
        }
        write_checkpoint(
            &checkpoint_path,
            change_name,
            &config.policy_hash,
            next,
            pre_completed + passed_count,
        );
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

/// Result of running a single slice through the implement→verify→review pipeline.
struct SliceResult {
    passed: bool,
}

/// Runs the implement→verify→review pipeline for one slice, gated by `semaphore`.
///
/// The semaphore permit is acquired before any work begins, bounding the number
/// of slices that run concurrently without preventing the futures from being
/// created all at once.
#[allow(clippy::too_many_arguments)]
async fn run_slice_parallel<'a, R: RoleRunner, S: StatusSink>(
    runner: &'a R,
    sink: &'a S,
    config: &'a LoopConfig,
    workspace: &'a Path,
    _change_name: &'a str,
    implementer: &'a LoopRole,
    fix: &'a LoopRole,
    reviewer: &'a LoopRole,
    timeout: std::time::Duration,
    max_attempts: u32,
    idx: usize,
    slice: &'a str,
    semaphore: Arc<tokio::sync::Semaphore>,
) -> SliceResult {
    // Acquire a concurrency slot; the permit is held for the slice's lifetime.
    let _permit = semaphore.acquire_owned().await.ok();

    #[allow(clippy::cast_possible_wrap)]
    sink.set_slice((idx + 1) as i64).await;

    let mut passed = false;
    for attempt in 0..max_attempts {
        let role = if attempt == 0 { implementer } else { fix };
        if run_role_traced(runner, role, idx, slice, attempt)
            .await
            .is_err()
        {
            break;
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
                let review_ok = run_role_traced(runner, reviewer, idx, slice, attempt)
                    .await
                    .is_ok();
                if config.review.required && !review_ok {
                    let _ = crate::mining::write_failure_guide(
                        "reviewer",
                        &[format!(
                            "slice {} (idx {idx}) rejected by reviewer",
                            idx + 1
                        )],
                        workspace,
                    );
                    break;
                }
            }
            passed = true;
            break;
        }
        sink.set_status(&LoopState::Fixed).await;
    }

    if !passed {
        let _ = crate::mining::write_failure_guide(
            fix.name.as_str(),
            &[format!(
                "slice {} (idx {idx}) failed verification after {max_attempts} attempt(s)",
                idx + 1
            )],
            workspace,
        );
    }

    SliceResult { passed }
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
