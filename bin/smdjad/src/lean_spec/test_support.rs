//! Shared test fixtures for the lean-spec submodules.
//!
//! Test-only (`#[cfg(test)]`): in-memory vault/ingot factories, the FNV test
//! embedder, and a namespace-scan helper the storage and context tests share.

use std::sync::Arc;

use smedja_ingot::IngotHandle;
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;

use crate::embedder_port::Embedder;

pub(crate) fn in_memory_vault() -> Arc<Mutex<Vault>> {
    Arc::new(Mutex::new(
        Vault::open_in_memory().expect("in-memory vault must open"),
    ))
}

/// Default FNV embedder shared by the lean-spec tests.
pub(crate) fn test_embedder() -> Arc<dyn Embedder> {
    Arc::new(crate::embedder_port::FnvEmbedder::new())
}

pub(crate) async fn entries_in(vault: &Arc<Mutex<Vault>>, namespace: &str) -> Vec<VaultEntry> {
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

pub(crate) fn in_memory_ingot() -> IngotHandle {
    IngotHandle::new(smedja_ingot::Ingot::open_in_memory().expect("in-memory ingot"))
}
