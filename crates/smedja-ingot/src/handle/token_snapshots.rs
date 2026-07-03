//! Token-snapshot handle methods.

use crate::{IngotError, IngotHandle, TokenSnapshot};
impl IngotHandle {
    // ── token_snapshots ───────────────────────────────────────────────────────

    /// Saves a [`TokenSnapshot`].
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying upsert, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn save_token_snapshot(&self, snap: TokenSnapshot) -> Result<(), IngotError> {
        self.run_blocking(move |ig| ig.save_token_snapshot(&snap))
            .await
    }

    /// Returns all [`TokenSnapshot`]s for `session_id`, ordered by `turn_n` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn session_token_snapshots(
        &self,
        session_id: &str,
    ) -> Result<Vec<TokenSnapshot>, IngotError> {
        let session_id = session_id.to_owned();
        self.run_blocking(move |ig| ig.session_token_snapshots(&session_id))
            .await
    }
}
