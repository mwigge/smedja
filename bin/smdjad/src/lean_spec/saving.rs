//! Self-measured lean-spec token savings.
//!
//! Records the tokens a slice did NOT spend by referencing its umbrella instead
//! of pasting it onto the tokens-saved ledger, tagged `source = "lean-spec"` so
//! the token-economy sibling proposal can attribute it.

use smedja_ingot::{IngotHandle, TokensSavedEntry};
use uuid::Uuid;

/// Ledger `source` tag attributed to the lean-spec saving so the token-economy
/// sibling proposal can separate it from `filter`/`crusher`/`cold-context`.
pub(crate) const LEAN_SPEC_SOURCE: &str = "lean-spec";

/// Records the lean-spec saving on the tokens-saved ledger.
///
/// The saving is the tokens a slice did NOT spend by referencing its umbrella
/// instead of pasting it: `saved = full_spec_paste_tokens −
/// umbrella_retrieved_tokens`, where the paste is the full umbrella text the
/// slice would otherwise restate and the retrieved cost is what the hybrid load
/// actually re-sent (the recalled detail; the intent is in the always-present
/// cached prefix). Both are measured with [`smedja_memory::estimate_tokens`].
///
/// Recorded only when the saving is positive, tagged `source = "lean-spec"` so
/// the token-economy sibling proposal can attribute it. A ledger error is logged
/// and swallowed — accounting is advisory and must never break the loop. Returns
/// the number of tokens recorded (`0` when nothing was recorded).
pub(crate) async fn record_lean_spec_saving(
    ingot: &IngotHandle,
    session_id: &str,
    full_spec_paste: &str,
    umbrella_retrieved: &str,
) -> u64 {
    let before = smedja_memory::estimate_tokens(full_spec_paste);
    let after = smedja_memory::estimate_tokens(umbrella_retrieved);
    let saved = before.saturating_sub(after);
    if saved == 0 {
        return 0;
    }
    let entry = TokensSavedEntry {
        id: Uuid::new_v4(),
        session_id: session_id.to_owned(),
        turn_n: 0,
        command: "lean-spec".to_owned(),
        tokens_saved: i64::try_from(saved).unwrap_or(i64::MAX),
        source: LEAN_SPEC_SOURCE.to_owned(),
        created_at: smedja_types::Timestamp::from_micros(0),
    };
    if let Err(e) = ingot.insert_tokens_saved(entry).await {
        tracing::warn!(error = %e, "failed to record lean-spec savings; continuing");
        return 0;
    }
    u64::try_from(saved).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lean_spec::test_support::in_memory_ingot;

    // ── group 5: self-measured savings (source=lean-spec) ───────────────────

    #[tokio::test]
    async fn lean_spec_saving_is_recorded_as_paste_minus_retrieved() {
        // Task 5.1: saved = full_spec_paste_tokens − umbrella_retrieved_tokens.
        let ingot = in_memory_ingot();
        let paste = "the full umbrella spec pasted in full ".repeat(20);
        let retrieved = "only the recalled detail fragment".to_owned();

        let recorded = record_lean_spec_saving(&ingot, "sess-1", &paste, &retrieved).await;

        let expected = smedja_memory::estimate_tokens(&paste)
            .saturating_sub(smedja_memory::estimate_tokens(&retrieved));
        assert_eq!(
            recorded,
            u64::try_from(expected).unwrap(),
            "saving must be paste − retrieved"
        );
        let total = ingot.session_tokens_saved("sess-1").await.unwrap();
        assert_eq!(
            total,
            i64::try_from(expected).unwrap(),
            "the saving must land on the ledger"
        );
    }

    #[tokio::test]
    async fn lean_spec_saving_recorded_only_when_positive() {
        // Task 5.2: nothing is recorded when retrieved ≥ paste (no saving).
        let ingot = in_memory_ingot();
        let paste = "short".to_owned();
        let retrieved = "a much longer retrieved body than the paste itself".to_owned();

        let recorded = record_lean_spec_saving(&ingot, "sess-2", &paste, &retrieved).await;

        assert_eq!(recorded, 0, "a non-positive saving must not be recorded");
        assert_eq!(
            ingot.session_tokens_saved("sess-2").await.unwrap(),
            0,
            "the ledger must hold no row for a non-positive saving"
        );
    }

    #[tokio::test]
    async fn lean_spec_saving_is_tagged_source_lean_spec() {
        // Task 5.3/5.4: the recorded saving carries source = "lean-spec" so the
        // token-economy sibling proposal can attribute it.
        let ingot = in_memory_ingot();
        let paste = "the full umbrella spec pasted in full ".repeat(20);
        let retrieved = "small fragment".to_owned();

        record_lean_spec_saving(&ingot, "sess-3", &paste, &retrieved).await;

        let by_source = ingot
            .session_tokens_saved_by_source("sess-3")
            .await
            .unwrap();
        assert!(
            by_source
                .iter()
                .any(|(src, n)| src == LEAN_SPEC_SOURCE && *n > 0),
            "the saving must be tagged source=lean-spec; got {by_source:?}"
        );
    }
}
