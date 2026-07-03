//! [`LoopStatusSink`]: persists loop progression through the ingot `loops` table.

use smedja_ingot::IngotHandle;
use smedja_loop::{LoopState, StatusSink};
use smedja_types::Timestamp;
use tracing::warn;

/// Persists loop progression through the ingot `loops` table.
pub(crate) struct LoopStatusSink {
    pub(crate) ingot: IngotHandle,
    pub(crate) loop_id: String,
}

impl StatusSink for LoopStatusSink {
    async fn set_status(&self, state: &LoopState) {
        if let Err(e) = self
            .ingot
            .update_loop_status(&self.loop_id, state.as_str(), Timestamp::now())
            .await
        {
            warn!(loop_id = %self.loop_id, error = %e, "failed to persist loop status");
        }
    }

    async fn set_slice(&self, slice: i64) {
        if let Err(e) = self
            .ingot
            .update_loop_slice(&self.loop_id, slice, Timestamp::now())
            .await
        {
            warn!(loop_id = %self.loop_id, error = %e, "failed to persist loop slice");
        }
    }
}
