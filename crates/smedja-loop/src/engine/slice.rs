//! Per-slice implement→verify→review pipeline.

use std::path::Path;
use std::sync::Arc;

use crate::config::LoopConfig;
use crate::role::LoopRole;
use crate::state::LoopState;
use crate::verify::run_verification;

use super::roles::run_role_traced;
use super::types::{RoleRunner, StatusSink};

/// Result of running a single slice through the implement→verify→review pipeline.
pub(crate) struct SliceResult {
    pub(crate) passed: bool,
}

/// Runs the implement→verify→review pipeline for one slice, gated by `semaphore`.
///
/// The semaphore permit is acquired before any work begins, bounding the number
/// of slices that run concurrently without preventing the futures from being
/// created all at once.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_slice_parallel<'a, R: RoleRunner, S: StatusSink>(
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
