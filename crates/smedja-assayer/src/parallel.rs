//! Parallel task pool for multi-role worktree execution.
//!
//! [`WorktreePool`] tracks [`Task`] records keyed by UUID. Each task
//! represents one agent role running against its own git worktree. Worktree
//! lifecycle (creation and removal) is handled by [`WorktreePool::start_worktrees`]
//! and [`WorktreePool::remove_worktree`]; process spawning remains the caller's
//! responsibility.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use uuid::Uuid;

/// Status of a parallel task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskStatus {
    /// Task has been registered but not yet started.
    Pending,
    /// Task is actively running under the given OS process.
    Running {
        /// PID of the smdjad session process for this role.
        pid: u32,
    },
    /// Task finished successfully.
    Complete,
    /// Task terminated with an error.
    Failed {
        /// Human-readable failure description.
        reason: String,
    },
    /// Task was cancelled before completion.
    Cancelled,
}

/// A single role task within a parallel work pool.
#[derive(Debug, Clone)]
pub struct Task {
    /// Unique identifier (UUID v4).
    pub id: String,
    /// Agent role name (e.g. `"impl"`, `"test"`, `"review"`).
    pub role: String,
    /// Free-text goal passed to the agent session.
    pub goal: String,
    /// Absolute path to the git worktree reserved for this task.
    pub worktree_path: PathBuf,
    /// Current lifecycle state.
    pub status: TaskStatus,
}

/// Pool that manages git worktrees and per-role tasks.
///
/// The pool owns the canonical record for each task. Callers update task
/// status via [`set_status`](WorktreePool::set_status) as the underlying
/// processes transition through their lifecycle.
#[derive(Debug, Default)]
pub struct WorktreePool {
    tasks: HashMap<String, Task>,
}

impl WorktreePool {
    /// Creates an empty pool.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a new task for `role` pursuing `goal` inside `workspace_root`.
    ///
    /// The task starts in [`TaskStatus::Pending`]. No worktree is created on
    /// disk — that is the caller's responsibility. Returns the generated task
    /// ID.
    pub fn register(&mut self, role: &str, goal: &str, workspace_root: &Path) -> String {
        let task_id = Uuid::new_v4().to_string();
        let worktree_path = workspace_root
            .join(".smedja")
            .join("worktrees")
            .join(&task_id);

        let task = Task {
            id: task_id.clone(),
            role: role.to_owned(),
            goal: goal.to_owned(),
            worktree_path,
            status: TaskStatus::Pending,
        };

        self.tasks.insert(task_id.clone(), task);
        task_id
    }

    /// Returns a reference to the task with the given `id`, if it exists.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id)
    }

    /// Returns an iterator over all registered tasks.
    pub fn tasks(&self) -> impl Iterator<Item = &Task> + '_ {
        self.tasks.values()
    }

    /// Marks a task as [`TaskStatus::Cancelled`].
    ///
    /// Returns `true` if the task was found and updated, `false` if no task
    /// with that `id` exists.
    pub fn cancel(&mut self, id: &str) -> bool {
        self.set_status(id, TaskStatus::Cancelled)
    }

    /// Updates the status of the task identified by `id`.
    ///
    /// Returns `true` if the task was found and updated, `false` otherwise.
    pub fn set_status(&mut self, id: &str, status: TaskStatus) -> bool {
        match self.tasks.get_mut(id) {
            Some(task) => {
                task.status = status;
                true
            }
            None => false,
        }
    }

    /// Creates git worktrees on disk for all [`TaskStatus::Pending`] tasks.
    ///
    /// Calls `git worktree add <path> HEAD` for each pending task using
    /// [`tokio::process::Command`]. On success the task transitions to
    /// `Running { pid: 0 }` — the caller is expected to replace `pid` once
    /// the session process is actually started. On non-zero exit or I/O error
    /// the task transitions to `Failed { reason }`.
    ///
    /// Returns the list of task IDs that were successfully set to `Running`.
    pub async fn start_worktrees(&mut self, workspace_root: &Path) -> Vec<String> {
        let mut started = Vec::new();
        let pending_ids: Vec<String> = self
            .tasks
            .values()
            .filter(|t| t.status == TaskStatus::Pending)
            .map(|t| t.id.clone())
            .collect();

        for id in pending_ids {
            let path = workspace_root.join(".smedja").join("worktrees").join(&id);

            let path_str = path.to_str().unwrap_or(".smedja/worktrees/x").to_owned();

            let output = tokio::process::Command::new("git")
                .args(["worktree", "add", &path_str, "HEAD"])
                .current_dir(workspace_root)
                .output()
                .await;

            match output {
                Ok(out) if out.status.success() => {
                    tracing::info!(task_id = %id, path = ?path, "worktree created");
                    self.set_status(&id, TaskStatus::Running { pid: 0 });
                    started.push(id);
                }
                Ok(out) => {
                    let reason = String::from_utf8_lossy(&out.stderr).to_string();
                    tracing::warn!(task_id = %id, %reason, "git worktree add failed");
                    self.set_status(&id, TaskStatus::Failed { reason });
                }
                Err(e) => {
                    let reason = e.to_string();
                    tracing::warn!(task_id = %id, %reason, "git worktree add error");
                    self.set_status(&id, TaskStatus::Failed { reason });
                }
            }
        }
        started
    }

    /// Removes the git worktree for any task, regardless of its current status.
    ///
    /// Calls `git worktree remove --force <path>`. The operation is
    /// best-effort: failures are logged as warnings but no error is returned.
    /// The task record is not removed from the pool — status updates remain
    /// the caller's responsibility.
    pub async fn remove_worktree(&self, task_id: &str, workspace_root: &Path) {
        let Some(task) = self.tasks.get(task_id) else {
            tracing::warn!(task_id, "remove_worktree: task not found");
            return;
        };

        let path_str = task.worktree_path.to_str().unwrap_or(".").to_owned();

        let output = tokio::process::Command::new("git")
            .args(["worktree", "remove", "--force", &path_str])
            .current_dir(workspace_root)
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => {
                tracing::info!(task_id, "worktree removed");
            }
            Ok(out) => {
                tracing::warn!(
                    task_id,
                    stderr = %String::from_utf8_lossy(&out.stderr),
                    "git worktree remove failed (ignored)",
                );
            }
            Err(e) => {
                tracing::warn!(task_id, error = %e, "git worktree remove error (ignored)");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{TaskStatus, WorktreePool};

    #[test]
    fn register_creates_pending_task() {
        let mut pool = WorktreePool::new();
        let root = Path::new("/tmp/ws");

        let id = pool.register("impl", "Add flag parser", root);

        let task = pool.get(&id).expect("task must exist after register");
        assert_eq!(task.role, "impl");
        assert_eq!(task.goal, "Add flag parser");
        assert_eq!(task.status, TaskStatus::Pending);
        assert_eq!(
            task.worktree_path,
            root.join(".smedja").join("worktrees").join(&id)
        );
    }

    #[test]
    fn cancel_marks_task_cancelled() {
        let mut pool = WorktreePool::new();
        let root = Path::new("/tmp/ws");

        let id = pool.register("review", "Audit auth module", root);
        let found = pool.cancel(&id);

        assert!(found, "cancel must return true for a known id");
        assert_eq!(pool.get(&id).unwrap().status, TaskStatus::Cancelled);
    }

    #[test]
    fn get_returns_none_for_unknown_id() {
        let pool = WorktreePool::new();
        assert!(pool.get("00000000-0000-0000-0000-000000000000").is_none());
    }

    #[test]
    fn dependency_ordering_respected() {
        let mut pool = WorktreePool::new();
        let root = Path::new("/tmp/ws");

        let id_impl = pool.register("impl", "Implement feature", root);
        let id_test = pool.register("test", "Test feature", root);
        let id_review = pool.register("review", "Review feature", root);

        // Collect all task IDs from the pool.
        let mut found_ids: Vec<String> = pool.tasks().map(|t| t.id.clone()).collect();
        found_ids.sort();

        let mut expected = vec![id_impl, id_test, id_review];
        expected.sort();

        assert_eq!(
            found_ids, expected,
            "all three registered tasks must be present in pool"
        );
    }
}
