//! Async facade over [`Ingot`].
//!
//! [`IngotHandle`] wraps a [`std::sync::Mutex`]-guarded [`Ingot`] in an
//! [`Arc`] so it is cheaply [`Clone`]-able across task boundaries. Every
//! method delegates to the corresponding [`Ingot`] method inside
//! [`tokio::task::spawn_blocking`] via [`IngotHandle::run_blocking`], keeping
//! `SQLite` I/O off the Tokio executor thread-pool.
use std::sync::Arc;

use crate::{Ingot, IngotError};

/// Converts a [`tokio::task::JoinError`] (a panic inside `spawn_blocking`) into
/// an [`IngotError::TaskPanic`] so callers see a uniform error type.
#[allow(clippy::needless_pass_by_value)] // used as `.map_err(join_err)`, which requires taking the error by value
fn join_err(e: tokio::task::JoinError) -> IngotError {
    IngotError::TaskPanic(e.to_string())
}

/// Async facade over [`Ingot`].
///
/// All methods route through [`tokio::task::spawn_blocking`] so `SQLite`
/// operations do not block Tokio executor threads. The handle is
/// cheaply [`Clone`]able — all clones share the same underlying database
/// connection.
#[derive(Clone)]
pub struct IngotHandle {
    inner: Arc<std::sync::Mutex<Ingot>>,
}

impl IngotHandle {
    /// Wraps `ingot` in an async handle.
    #[must_use]
    pub fn new(ingot: Ingot) -> Self {
        Self {
            inner: Arc::new(std::sync::Mutex::new(ingot)),
        }
    }

    /// Runs `f` against the guarded [`Ingot`] on a blocking thread.
    ///
    /// Clones the shared [`Arc`], moves `f` onto a [`tokio::task::spawn_blocking`]
    /// thread, locks the mutex, and invokes `f` with a shared reference to the
    /// [`Ingot`]. A poisoned lock (left behind by a prior panic) is recovered via
    /// [`std::sync::PoisonError::into_inner`]: the underlying `SQLite` connection
    /// remains valid after a panic because no operation leaves it in a torn
    /// state, so the guard is reused rather than propagating the poison.
    ///
    /// # Errors
    ///
    /// Returns whatever [`IngotError`] `f` produces, or [`IngotError::TaskPanic`]
    /// if the blocking task itself panics.
    async fn run_blocking<T, F>(&self, f: F) -> Result<T, IngotError>
    where
        F: FnOnce(&Ingot) -> Result<T, IngotError> + Send + 'static,
        T: Send + 'static,
    {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let guard = inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            f(&guard)
        })
        .await
        .map_err(join_err)?
    }
}

mod audit;
mod checkpoints;
mod cost;
mod jsonl;
mod loops;
mod mcp;
mod prompt_hashes;
mod rollups;
mod sessions;
mod tasks;
mod token_snapshots;

#[cfg(test)]
mod tests {
    use super::IngotHandle;
    use crate::{Ingot, IngotError, Session};
    use smedja_types::Timestamp;
    use uuid::Uuid;

    fn make_handle() -> IngotHandle {
        let ingot = Ingot::open_in_memory().expect("in-memory db failed");
        IngotHandle::new(ingot)
    }

    fn sample_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_secs_f64(1_700_000_000.0),
            updated_at: Timestamp::from_secs_f64(1_700_000_000.0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    #[tokio::test]
    async fn ingot_handle_get_session_returns_none_for_unknown_id() {
        let handle = make_handle();
        let result = handle.get_session("nonexistent-id").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn ingot_handle_save_and_load_session_roundtrip() {
        let handle = make_handle();
        let session = sample_session();
        let id = session.id.to_string();

        handle.create_session(session.clone()).await.unwrap();

        let fetched = handle.get_session(&id).await.unwrap();
        assert!(fetched.is_some());
        let fetched = fetched.unwrap();
        assert_eq!(fetched.id, session.id);
        assert_eq!(fetched.status, "active");
    }

    #[tokio::test]
    async fn run_blocking_panic_surfaces_task_panic() {
        let handle = make_handle();
        let result: Result<(), IngotError> = handle
            .run_blocking(|_ig| panic!("boom inside blocking closure"))
            .await;
        match result {
            Err(IngotError::TaskPanic(_)) => {}
            other => panic!("expected TaskPanic, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn poisoned_lock_recovers_and_subsequent_calls_succeed() {
        let handle = make_handle();

        // Poison the mutex by panicking while the lock is held.
        let panicked: Result<(), IngotError> =
            handle.run_blocking(|_ig| panic!("poison the lock")).await;
        assert!(matches!(panicked, Err(IngotError::TaskPanic(_))));

        // The connection remains valid: a subsequent operation must still work
        // because run_blocking recovers the poisoned guard via into_inner().
        let session = sample_session();
        let id = session.id.to_string();
        handle
            .create_session(session)
            .await
            .expect("operation after poison must succeed");
        let fetched = handle.get_session(&id).await.unwrap();
        assert!(fetched.is_some(), "row written after poison recovery");
    }
}
