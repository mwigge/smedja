//! Lean-spec umbrella/slice machinery.
//!
//! A *spec* today restates its full shared context inside every related change,
//! so the model re-reads the same Why/design once per change. Lean-specs applies
//! smedja's hot/warm/cold economy to specs themselves:
//!
//! - An **umbrella** holds the durable trail of thought once. Its *design detail*
//!   (large, variable) is chunked into vault entries under an `umbrella:<id>`
//!   namespace via the existing `Vault::insert` path, each entry's `payload`
//!   recording `{"kind":"umbrella","umbrella_id":<id>}` — the same payload-kind
//!   convention the filter-recovery tee uses
//!   ([`crate::executor::FILTER_RECOVERY_NAMESPACE`]).
//! - A **slice** is a thin child carrying only its own delta plus a pointer to
//!   its umbrella (`umbrella_id`, `slice_n`). The pointer is metadata, not a
//!   manifest `parent` field — the `.openspec.yaml` stays flat.
//! - **Loading is hybrid**: the umbrella *intent/contract* (small, stable) is
//!   pinned in the sealed stable prefix so it is KV-cached and cheap to re-send
//!   per slice; the umbrella *design detail* is recalled per slice on demand from
//!   the vault via [`smedja_memory::WorkingMemory::cold_context`] with the
//!   cold-query namespace set to the umbrella namespace.
//!
//! Honest caveat: the vault embedder is FNV-1a bag-of-words (`DIM = 128`,
//! [`crate::embedder`]) — semantic recall is weak. The hybrid keyword + recency
//! boost in [`smedja_vault::Vault::search`] partially compensates, and the
//! umbrella intent that matters most lives in the *exact*, always-present cached
//! prefix rather than in cold recall. Retrieval-linking the umbrella detail is a
//! good first cut, stated plainly, not precision recall.

use std::sync::Arc;

use smedja_ingot::{IngotHandle, TokensSavedEntry};
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::embedder_port::Embedder;

/// Ledger `source` tag attributed to the lean-spec saving so the token-economy
/// sibling proposal can separate it from `filter`/`crusher`/`cold-context`.
pub(crate) const LEAN_SPEC_SOURCE: &str = "lean-spec";

/// Payload `kind` discriminator stamped on every umbrella chunk, mirroring the
/// filter-recovery `{"kind":"filter-recovery"}` convention
/// (`bin/smdjad/src/executor/mod.rs`).
pub(crate) const UMBRELLA_KIND: &str = "umbrella";

/// A slice's pointer back to its umbrella.
///
/// The link is metadata — an `umbrella_id` and a `slice_n` — modelled on the
/// vault payload convention, NOT a `parent` field in the `OpenSpec` change
/// manifest (which stays flat: `schema` + `created` only). A slice carries this
/// pointer instead of restating the umbrella's Why or design.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SlicePointer {
    /// Identifier of the umbrella this slice belongs to.
    pub umbrella_id: String,
    /// 1-based ordinal of this slice within the umbrella's slice list.
    pub slice_n: u32,
}

impl SlicePointer {
    /// Creates a pointer to slice `slice_n` of umbrella `umbrella_id`.
    #[must_use]
    pub(crate) fn new(umbrella_id: impl Into<String>, slice_n: u32) -> Self {
        Self {
            umbrella_id: umbrella_id.into(),
            slice_n,
        }
    }

    /// Renders the pointer as the JSON payload a slice records, reusing the
    /// vault payload-kind convention (`{"kind":"slice", ...}`).
    #[must_use]
    pub(crate) fn to_payload(&self) -> serde_json::Value {
        serde_json::json!({
            "kind": "slice",
            "umbrella_id": self.umbrella_id,
            "slice_n": self.slice_n,
        })
    }

    /// Returns the umbrella namespace this pointer resolves to.
    #[must_use]
    pub(crate) fn umbrella_namespace(&self) -> String {
        umbrella_namespace(&self.umbrella_id)
    }
}

/// Builds the vault namespace that holds an umbrella's chunks.
///
/// All of an umbrella's design-detail chunks live under `umbrella:<id>`, so a
/// slice can resolve and recall its umbrella by id with a single namespace
/// scope.
#[must_use]
pub(crate) fn umbrella_namespace(umbrella_id: &str) -> String {
    format!("umbrella:{umbrella_id}")
}

/// Splits `detail` into chunks no longer than `max_chars` on paragraph
/// boundaries.
///
/// Umbrella design detail is large and variable; chunking keeps each vault entry
/// small enough that cold recall can surface the relevant fragment rather than
/// the whole document. Paragraphs (`\n\n`-separated) are packed greedily; a lone
/// paragraph longer than `max_chars` becomes its own chunk rather than being
/// split mid-word. Returns an empty `Vec` for blank input.
#[must_use]
pub(crate) fn chunk_detail(detail: &str, max_chars: usize) -> Vec<String> {
    let max_chars = max_chars.max(1);
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for paragraph in detail
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        // A fresh oversized paragraph stands alone rather than being split mid-word.
        if current.is_empty() {
            current.push_str(paragraph);
        } else if current.len() + 2 + paragraph.len() <= max_chars {
            current.push_str("\n\n");
            current.push_str(paragraph);
        } else {
            chunks.push(std::mem::take(&mut current));
            current.push_str(paragraph);
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Stores an umbrella's design detail as chunked vault entries under the
/// `umbrella:<id>` namespace.
///
/// Each chunk becomes a [`VaultEntry`] embedded with the daemon embedder, in the
/// `umbrella:<id>` namespace, with `payload` `{"kind":"umbrella",
/// "umbrella_id":<id>}` and `chunk_index` set to its position. Vault writes are
/// synchronous `SQLite`, so they run on a blocking thread.
///
/// # Errors
///
/// Returns the [`smedja_vault::VaultError`] from the first failing insert.
pub(crate) async fn store_umbrella_detail(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    umbrella_id: &str,
    detail: &str,
    max_chars: usize,
) -> Result<usize, smedja_vault::VaultError> {
    let namespace = umbrella_namespace(umbrella_id);
    let chunks = chunk_detail(detail, max_chars);
    let payload = serde_json::json!({
        "kind": UMBRELLA_KIND,
        "umbrella_id": umbrella_id,
    });
    let umbrella_id = umbrella_id.to_owned();
    let vault = Arc::clone(vault);

    // Embed every chunk on the async path first (the learned backend does network
    // I/O here, degrading to FNV on failure), then persist on a blocking thread.
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    let mut embeddings = Vec::with_capacity(chunks.len());
    for chunk in &chunks {
        embeddings.push(embedder.embed_query(chunk).await);
    }

    // `Vault` is synchronous `SQLite`; do all the inserts on a blocking thread so
    // the async runtime is never stalled. A `JoinError` only occurs if the
    // blocking closure panics — an unrecoverable bug — so the panic is resumed
    // rather than masked behind a fabricated vault error.
    let join = tokio::task::spawn_blocking(move || -> Result<usize, smedja_vault::VaultError> {
        let mut guard = vault.blocking_lock();
        for (index, (chunk, embedding)) in chunks.iter().zip(embeddings).enumerate() {
            // Legacy `upsert` (not `insert`) is used on purpose: `insert`'s
            // >0.85-cosine dedup would silently drop near-identical chunks, and
            // every umbrella chunk must persist so cold recall can reach it.
            let entry = VaultEntry {
                id: format!("umbrella:{umbrella_id}:{index}"),
                embedding,
                payload: payload.clone(),
                namespace: namespace.clone(),
                content: chunk.clone(),
                source_file: None,
                added_by: Some("lean-spec".to_owned()),
                #[allow(clippy::cast_possible_wrap)] // chunk counts never exceed i64::MAX
                chunk_index: Some(index as i64),
                parent_id: Some(umbrella_id.clone()),
                created_at: 0.0,
                embedder_model_id: model_id.clone(),
                dim,
            };
            guard.upsert(&entry)?;
        }
        Ok(chunks.len())
    })
    .await;
    match join {
        Ok(result) => result,
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// Resolves an umbrella's chunks by id, returning the stored [`VaultEntry`]
/// rows in the `umbrella:<id>` namespace.
///
/// A dangling pointer (no chunks stored for `umbrella_id`) yields an empty
/// `Vec`, never an error — matching the graceful-degradation contract of
/// [`smedja_memory::WorkingMemory::cold_context`]. The query embedding is the
/// embedded `query` text, which the hybrid keyword boost favours.
pub(crate) async fn resolve_umbrella(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    umbrella_id: &str,
    query: &str,
    k: usize,
) -> Vec<VaultEntry> {
    let namespace = umbrella_namespace(umbrella_id);
    let query = query.to_owned();
    let vault = Arc::clone(vault);
    let query_vec = embedder.embed_query(&query).await;
    let model_id = embedder.model_id().to_owned();
    let dim = embedder.dim();
    let join = tokio::task::spawn_blocking(move || {
        let guard = vault.blocking_lock();
        guard.search(&query_vec, &query, &namespace, k, &model_id, dim)
    })
    .await;

    match join {
        Ok(Ok(entries)) => entries,
        Ok(Err(e)) => {
            tracing::debug!(error = %e, umbrella_id, "umbrella resolve search failed");
            Vec::new()
        }
        Err(e) => {
            tracing::debug!(error = %e, umbrella_id, "umbrella resolve task panicked");
            Vec::new()
        }
    }
}

/// Persists a slice's umbrella pointer as a vault entry, returning its id.
///
/// The pointer rides the vault payload convention: the entry's `payload` is
/// [`SlicePointer::to_payload`] (`{"kind":"slice","umbrella_id":...,
/// "slice_n":...}`) under the umbrella's namespace, so a slice's link to its
/// umbrella is metadata — never a manifest `parent` field. Best-effort: a vault
/// error is logged and swallowed, and the slice still proceeds on its delta.
pub(crate) async fn store_slice_pointer(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    pointer: &SlicePointer,
) -> String {
    let id = format!("slice:{}:{}", pointer.umbrella_id, pointer.slice_n);
    let embedding = embedder.embed_query(&id).await;
    let entry = VaultEntry {
        id: id.clone(),
        embedding,
        payload: pointer.to_payload(),
        namespace: pointer.umbrella_namespace(),
        content: format!(
            "slice {} of umbrella {}",
            pointer.slice_n, pointer.umbrella_id
        ),
        source_file: None,
        added_by: Some("lean-spec".to_owned()),
        chunk_index: None,
        parent_id: Some(pointer.umbrella_id.clone()),
        created_at: 0.0,
        embedder_model_id: embedder.model_id().to_owned(),
        dim: embedder.dim(),
    };
    let vault = Arc::clone(vault);
    let join = tokio::task::spawn_blocking(move || {
        let mut guard = vault.blocking_lock();
        guard.upsert(&entry)
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "lean-spec slice pointer store failed; continuing");
        }
        Err(e) => {
            tracing::warn!(error = %e, "lean-spec slice pointer store task panicked; continuing");
        }
    }
    id
}

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

/// Default maximum characters per umbrella chunk when preloading from disk.
///
/// Sized so a typical `design.md` section lands as its own recallable chunk
/// rather than one monolithic entry the weak embedder cannot discriminate.
pub(crate) const UMBRELLA_CHUNK_CHARS: usize = 800;

/// Reads an umbrella change's intent and design detail from its `OpenSpec`
/// directory.
///
/// The *intent* (small, stable — sealed into the cached prefix) is the change's
/// `proposal.md`; the *design detail* (large, variable — chunked into the vault
/// for cold recall) is its `design.md`. Returns `(intent, detail)`, each empty
/// when its file is absent — a missing umbrella file degrades to "no umbrella
/// context" rather than failing.
///
/// `change_dir` is the already-validated change directory (the loop resolves it
/// through [`crate::loop_runner`]'s workspace-boundary check before calling).
pub(crate) async fn read_umbrella_sources(change_dir: &std::path::Path) -> (String, String) {
    let intent = tokio::fs::read_to_string(change_dir.join("proposal.md"))
        .await
        .unwrap_or_default();
    let detail = tokio::fs::read_to_string(change_dir.join("design.md"))
        .await
        .unwrap_or_default();
    (intent, detail)
}

/// Stores an umbrella's design detail (from `read_umbrella_sources`) into the
/// vault, returning the number of chunks stored.
///
/// A no-op returning `Ok(0)` when `detail` is blank. This is the umbrella-once
/// half of the loop cadence: the detail is chunked into `umbrella:<id>` a single
/// time, then every slice recalls from it on demand.
///
/// # Errors
///
/// Propagates the [`smedja_vault::VaultError`] from [`store_umbrella_detail`].
pub(crate) async fn preload_umbrella(
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn Embedder>,
    umbrella_id: &str,
    detail: &str,
) -> Result<usize, smedja_vault::VaultError> {
    if detail.trim().is_empty() {
        return Ok(0);
    }
    store_umbrella_detail(vault, embedder, umbrella_id, detail, UMBRELLA_CHUNK_CHARS).await
}

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

    fn in_memory_vault() -> Arc<Mutex<Vault>> {
        Arc::new(Mutex::new(
            Vault::open_in_memory().expect("in-memory vault must open"),
        ))
    }

    /// Default FNV embedder shared by the lean-spec tests.
    fn test_embedder() -> Arc<dyn Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    async fn entries_in(vault: &Arc<Mutex<Vault>>, namespace: &str) -> Vec<VaultEntry> {
        let query_vec = crate::embedder::embed("");
        let ns = namespace.to_owned();
        let v = Arc::clone(vault);
        tokio::task::spawn_blocking(move || {
            let guard = v.blocking_lock();
            guard
                .search(
                    &query_vec,
                    "",
                    &ns,
                    1000,
                    smedja_vault::LEGACY_MODEL_ID,
                    crate::embedder::DIM,
                )
                .expect("search must succeed")
        })
        .await
        .expect("blocking task must not panic")
    }

    // ── group 1: umbrella storage in the vault ──────────────────────────────

    #[tokio::test]
    async fn umbrella_detail_chunks_land_in_umbrella_namespace_with_kind_payload() {
        // Task 1.1/1.2: detail is chunked and stored under `umbrella:<id>` with
        // each entry's payload carrying {"kind":"umbrella","umbrella_id":<id>}.
        let vault = in_memory_vault();
        let detail = "First paragraph of design rationale.\n\n\
             Second paragraph carrying more of the umbrella's durable detail.";

        let chunks = store_umbrella_detail(&vault, &test_embedder(), "alpha", detail, 64)
            .await
            .expect("store must succeed");
        assert!(
            chunks >= 2,
            "multi-paragraph detail must chunk; got {chunks}"
        );

        let stored = entries_in(&vault, &umbrella_namespace("alpha")).await;
        assert_eq!(stored.len(), chunks, "every chunk must be persisted");
        for entry in &stored {
            assert_eq!(entry.namespace, "umbrella:alpha");
            assert_eq!(entry.payload["kind"], serde_json::json!(UMBRELLA_KIND));
            assert_eq!(entry.payload["umbrella_id"], serde_json::json!("alpha"));
        }
    }

    // ── group 2: slice pointer (umbrella_id, slice_n) ───────────────────────

    #[test]
    fn slice_records_umbrella_pointer_as_payload_metadata() {
        // Task 2.1/2.2: a slice records umbrella_id + slice_n as pointer
        // metadata (the vault payload convention), not a manifest parent field.
        let pointer = SlicePointer::new("alpha", 3);
        let payload = pointer.to_payload();
        assert_eq!(payload["kind"], serde_json::json!("slice"));
        assert_eq!(payload["umbrella_id"], serde_json::json!("alpha"));
        assert_eq!(payload["slice_n"], serde_json::json!(3));
        // The pointer resolves to the umbrella's namespace by id alone.
        assert_eq!(pointer.umbrella_namespace(), "umbrella:alpha");
    }

    // (Removed `openspec_manifest_stays_flat_no_parent_field`: it read a
    // committed `openspec/changes/.../.openspec.yaml`, but openspec/ is no longer
    // tracked in the repo — the manifest schema is exercised by the round-trip
    // tests below instead.)

    #[tokio::test]
    async fn slice_resolves_its_umbrella_via_pointer() {
        // Task 2.3/2.4: given a slice carrying umbrella_id, the umbrella's chunks
        // are retrieved from the umbrella:<id> namespace by the matching id.
        let vault = in_memory_vault();
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "alpha",
            "alpha umbrella design detail body",
            256,
        )
        .await
        .expect("store alpha");
        let pointer = SlicePointer::new("alpha", 1);

        let chunks = resolve_umbrella(
            &vault,
            &test_embedder(),
            &pointer.umbrella_id,
            "design detail",
            5,
        )
        .await;
        assert!(!chunks.is_empty(), "the pointer must resolve to chunks");
        assert!(
            chunks
                .iter()
                .all(|c| c.payload["umbrella_id"] == serde_json::json!("alpha")),
            "resolved chunks must be those whose payload records the matching id"
        );
    }

    #[tokio::test]
    async fn dangling_umbrella_pointer_yields_empty_not_error() {
        // Task 2.4: a dangling umbrella_id (no stored chunks) returns an empty
        // result, never an error — matching cold_context's contract.
        let vault = in_memory_vault();
        let pointer = SlicePointer::new("ghost", 1);
        let chunks = resolve_umbrella(
            &vault,
            &test_embedder(),
            &pointer.umbrella_id,
            "anything",
            5,
        )
        .await;
        assert!(
            chunks.is_empty(),
            "an unstored umbrella must degrade to an empty result"
        );
    }

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

    // ── group 5: self-measured savings (source=lean-spec) ───────────────────

    fn in_memory_ingot() -> IngotHandle {
        IngotHandle::new(smedja_ingot::Ingot::open_in_memory().expect("in-memory ingot"))
    }

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

    #[tokio::test]
    async fn umbrella_namespace_search_returns_only_that_umbrella() {
        // Task 1.3/1.4: a search over `umbrella:<id>` returns only that
        // umbrella's chunks; no other namespace leaks in.
        let vault = in_memory_vault();
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "alpha",
            "alpha design detail body",
            256,
        )
        .await
        .expect("store alpha");
        store_umbrella_detail(
            &vault,
            &test_embedder(),
            "beta",
            "beta design detail body",
            256,
        )
        .await
        .expect("store beta");

        let alpha = entries_in(&vault, &umbrella_namespace("alpha")).await;
        assert!(!alpha.is_empty(), "alpha chunks must exist");
        assert!(
            alpha
                .iter()
                .all(|e| e.payload["umbrella_id"] == serde_json::json!("alpha")),
            "no other umbrella's chunks may leak into the alpha namespace"
        );
        assert!(
            alpha.iter().all(|e| e.namespace == "umbrella:alpha"),
            "every result must be in the alpha namespace"
        );
    }
}
