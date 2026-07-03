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
//!
//! ## Module layout
//!
//! - [`model`]: the [`SlicePointer`] metadata, the `umbrella:<id>` namespace, and
//!   paragraph-boundary detail chunking (pure, dependency-free primitives).
//! - [`storage`]: vault-backed umbrella storage, recall, slice-pointer
//!   persistence, and reading umbrella sources off an `OpenSpec` directory.
//! - [`context`]: hybrid per-slice context assembly and the restatement guard.
//! - [`saving`]: self-measured token savings on the tokens-saved ledger.

mod context;
mod model;
mod saving;
mod storage;

#[cfg(test)]
mod test_support;

pub(crate) use context::{assemble_slice_context, default_slice_recall_k, slice_restates_intent};
pub(crate) use model::SlicePointer;
pub(crate) use saving::record_lean_spec_saving;
pub(crate) use storage::{
    preload_umbrella, read_umbrella_sources, resolve_umbrella, store_slice_pointer,
};
