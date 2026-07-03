//! Prompt-hash handle methods.

use crate::{IngotError, IngotHandle, PromptHashRecord};
impl IngotHandle {
    // ── prompt_hashes ─────────────────────────────────────────────────────────

    /// Records a prompt content hash for `(change, role)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying INSERT, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn save_prompt_hash(
        &self,
        change: &str,
        role: &str,
        hash: &str,
    ) -> Result<(), IngotError> {
        let change = change.to_owned();
        let role = role.to_owned();
        let hash = hash.to_owned();
        self.run_blocking(move |ig| ig.save_prompt_hash(&change, &role, &hash))
            .await
    }

    /// Returns the most recent prompt hash for `(change, role)`.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn get_prompt_hash(
        &self,
        change: &str,
        role: &str,
    ) -> Result<Option<String>, IngotError> {
        let change = change.to_owned();
        let role = role.to_owned();
        self.run_blocking(move |ig| ig.get_prompt_hash(&change, &role))
            .await
    }

    /// Returns all prompt hash records for `change`, ordered by `ts` ascending.
    ///
    /// # Errors
    ///
    /// Propagates [`IngotError::Db`] from the underlying query, or
    /// [`IngotError::TaskPanic`] if the blocking task panics.
    pub async fn list_prompt_hashes(
        &self,
        change: &str,
    ) -> Result<Vec<PromptHashRecord>, IngotError> {
        let change = change.to_owned();
        self.run_blocking(move |ig| ig.list_prompt_hashes(&change))
            .await
    }
}
