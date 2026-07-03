//! Hybrid per-slice context assembly and the restatement guard.
//!
//! A slice's context is loaded in three moves: the umbrella intent is sealed into
//! the KV-cached prefix, the umbrella detail is recalled per slice from the vault
//! cold stratum, and the slice's own delta lands in the mutable window
//! (`assemble_slice_context`). `slice_restates_intent` is the authoring guard that
//! flags a slice which copies the umbrella's Why/design instead of referencing it.

use std::sync::Arc;

use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::embedder_port::Embedder;
use crate::lean_spec::model::SlicePointer;

/// Assembled hybrid context for a single slice.
///
/// Reports the boundary so callers (and tests) can prove which messages were
/// sealed (umbrella intent, KV-cached and cheap to re-send) versus which fall in
/// the mutable window (the recalled umbrella detail and the slice's own delta).
pub(crate) struct SliceContext {
    /// The working memory whose stable prefix holds the umbrella intent and
    /// whose mutable window holds the recalled detail plus the slice delta.
    pub memory: smedja_memory::WorkingMemory,
    /// Number of umbrella-detail chunks recalled from the vault for this slice.
    pub detail_chunks: usize,
}

/// Assembles a slice's hybrid context: umbrella intent in the sealed cached
/// prefix, umbrella detail recalled per slice from the vault, slice delta in the
/// mutable window.
///
/// The sequence is the crux of lean-specs:
/// 1. Push the umbrella *intent/contract* (small, stable) and `seal_prefix()` so
///    it is KV-cached and re-sent cheaply on every slice.
/// 2. Set the cold-query namespace to the umbrella's `umbrella:<id>` and recall
///    its *design detail* (large, variable) via `cold_context` — the vault's
///    cold stratum. The detail lands in the mutable window, recalled per slice
///    rather than re-sent in full.
/// 3. Push the slice's own `slice_delta` into the mutable window.
///
/// `intent` is the only text sealed; `slice_delta` is the slice's own content
/// and MUST NOT restate the umbrella's Why/design (see [`slice_restates_intent`]).
pub(crate) async fn assemble_slice_context(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    pointer: &SlicePointer,
    intent: &str,
    slice_delta: &str,
    cold_k: usize,
) -> SliceContext {
    use smedja_memory::{Message, WorkingMemory};

    let cold_store = Arc::new(crate::orchestrator::cold::VaultColdStore::new(
        Arc::clone(vault),
        Arc::clone(embedder),
    ));
    let mut memory = WorkingMemory::new(usize::MAX).with_cold_store(cold_store);

    // 1. Umbrella intent → sealed cached prefix (KV-cached, re-sent per slice).
    memory.push(Message::system(intent.to_owned()));
    memory.seal_prefix();

    // 2. Umbrella detail → recalled per slice from the vault cold stratum,
    //    scoped to this umbrella's namespace. The slice delta is the query so the
    //    hybrid keyword boost favours the chunks the slice actually touches.
    memory.set_cold_query(pointer.umbrella_namespace(), cold_k);
    let detail = memory.cold_context(slice_delta).await;
    let detail_chunks = detail.len();
    for chunk in detail {
        memory.push(chunk);
    }

    // 3. Slice delta → mutable window (thin, after the boundary).
    memory.push(Message::user(slice_delta.to_owned()));

    SliceContext {
        memory,
        detail_chunks,
    }
}

/// Returns `true` when `slice_delta` restates any line of the umbrella `intent`.
///
/// A slice MUST NOT copy the umbrella's Why/design; the umbrella context is
/// supplied by reference (cached prefix + cold recall), not duplicated inside the
/// slice. This is a best-effort authoring guard: it flags when a non-trivial
/// intent line reappears verbatim in the slice body.
#[must_use]
pub(crate) fn slice_restates_intent(intent: &str, slice_delta: &str) -> bool {
    /// A line shorter than this is too generic (e.g. a heading) to count as a
    /// restatement of the umbrella's Why/design.
    const MIN_MEANINGFUL_LEN: usize = 12;

    let slice_lines: Vec<&str> = slice_delta.lines().map(str::trim).collect();
    intent
        .lines()
        .map(str::trim)
        .filter(|line| line.len() >= MIN_MEANINGFUL_LEN)
        .any(|intent_line| slice_lines.contains(&intent_line))
}

/// Top-K umbrella-detail chunks recalled per slice during cold retrieval.
///
/// Small by design: the umbrella *intent* a slice depends on lives in the exact
/// cached prefix, so cold recall only supplies a few supplementary detail
/// fragments. Keeping K low bounds the per-slice retrieved cost the saving is
/// measured against.
#[must_use]
pub(crate) fn default_slice_recall_k() -> usize {
    3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lean_spec::storage::store_umbrella_detail;
    use crate::lean_spec::test_support::{in_memory_vault, test_embedder};

    // ── group 3: hybrid slice context loading (prefix + vault) ──────────────

    #[tokio::test]
    async fn umbrella_intent_sits_inside_sealed_prefix_slice_delta_after() {
        // Task 3.1/3.2: umbrella intent is pushed before seal_prefix() so it
        // falls inside stable_prefix(); the slice delta is in the mutable window.
        let vault = in_memory_vault();
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "alpha",
            "alpha design detail body text",
            256,
        )
        .await
        .expect("store alpha");
        let pointer = SlicePointer::new("alpha", 1);

        let assembled = assemble_slice_context(
            &vault,
            &test_embedder(),
            &pointer,
            "UMBRELLA INTENT",
            "SLICE DELTA",
            5,
        )
        .await;
        let mem = &assembled.memory;

        // The intent is within the sealed prefix.
        let prefix = &mem.messages()[..mem.stable_prefix()];
        assert!(
            prefix.iter().any(|m| m.content.contains("UMBRELLA INTENT")),
            "umbrella intent must be inside the sealed prefix"
        );
        // The slice delta is in the mutable window after the boundary.
        assert!(
            mem.mutable_window()
                .iter()
                .any(|m| m.content.contains("SLICE DELTA")),
            "slice delta must be in the mutable window after the boundary"
        );
        assert!(
            !prefix.iter().any(|m| m.content.contains("SLICE DELTA")),
            "the slice delta must not be sealed into the prefix"
        );
    }

    #[tokio::test]
    async fn slice_loads_intent_from_prefix_and_detail_from_vault() {
        // Task 3.3/3.4: detail is recalled on demand via cold_context with the
        // cold-query namespace set to umbrella:<id>; intent and detail both
        // appear in one assembly.
        let vault = in_memory_vault();
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "alpha",
            "rust async tokio runtime scheduling executor design detail",
            512,
        )
        .await
        .expect("store alpha");
        let pointer = SlicePointer::new("alpha", 1);

        let assembled = assemble_slice_context(
            &vault,
            &test_embedder(),
            &pointer,
            "intent: rust async tokio",
            "slice: add executor metric",
            5,
        )
        .await;
        let mem = &assembled.memory;

        assert!(assembled.detail_chunks >= 1, "detail must be recalled");
        // Intent from the cached prefix.
        assert!(
            mem.messages()[..mem.stable_prefix()]
                .iter()
                .any(|m| m.content.contains("intent: rust async tokio")),
            "intent must come from the cached prefix"
        );
        // Detail from the vault, in the mutable window.
        assert!(
            mem.mutable_window()
                .iter()
                .any(|m| m.content.contains("design detail")),
            "umbrella detail must be recalled into the mutable window"
        );
    }

    #[tokio::test]
    async fn slice_does_not_restate_the_umbrella() {
        // Task 3.5/3.6: the slice's own content excludes the umbrella's
        // Why/design; the umbrella appears only via cached prefix + cold recall.
        let intent = "Why: shared context is paid once.\nDesign: seal intent, recall detail.";
        let clean_slice = "Delta: add the executor latency metric.";
        let restating_slice = "Design: seal intent, recall detail.\nDelta: add metric.";

        assert!(
            !slice_restates_intent(intent, clean_slice),
            "a thin slice delta must not be flagged"
        );
        assert!(
            slice_restates_intent(intent, restating_slice),
            "a slice that copies an umbrella design line must be flagged"
        );
    }
}
