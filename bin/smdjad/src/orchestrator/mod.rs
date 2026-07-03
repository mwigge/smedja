//! Turn orchestration logic extracted from `run_turn` in `main.rs`.
//!
//! [`TurnOrchestrator`] encapsulates all the dependencies that were previously
//! threaded through the free function `run_turn` as parameters.  Call
//! [`TurnOrchestrator::run`] to execute a single agent turn end-to-end.
//!
//! The implementation is split across sibling submodules:
//! - [`run`] — the end-to-end turn pipeline ([`TurnOrchestrator::run`]).
//! - [`prep`] — per-turn prompt / tool / user-content builders.
//! - [`prompt`] — pure prompt- and context-block helpers.
//! - [`budget`] — context-pressure and summarisation helpers.
//! - [`persist`] — post-loop cost / checkpoint / snapshot / summarise steps.
//! - [`context`] — retention strata, cache-strategy, and outcome classification.
//! - [`cold`] — cold-context vault store and block assembly.

use std::collections::HashMap;
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::cowork::CoworkGate;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

pub(crate) mod cold;
mod context;

mod budget;
mod persist;
mod prep;
mod prompt;
mod run;

/// Shared map from session-resume keys to provider-native resume identifiers.
///
/// Constructed once in `main()` and threaded explicitly to every orchestrator
/// (replacing the former process-static `OnceLock` singleton) so tests can
/// supply their own map.
pub(crate) type ProviderSessions = Arc<Mutex<HashMap<String, String>>>;

/// Key identifying a persisted [`smedja_memory::CacheAligner`]: `(session_id, runner_name)`.
///
/// Keyed by runner as well as session because a [`smedja_memory::CacheHint`]
/// targets one specific provider's warm cache; a `provider-failover` runner
/// rotation must not smear one provider's prefix-digest history onto another.
pub(crate) type AlignerKey = (String, String);

/// Shared map from `(session_id, runner)` to its persisted cross-turn aligner.
///
/// Constructed once in `main()` and threaded to every orchestrator exactly like
/// [`ProviderSessions`], so a single aligner instance outlives an individual turn
/// and can observe the prior sealed prefix to report real `Grown`/`Mutated` drift.
pub(crate) type CacheAligners = Arc<Mutex<HashMap<AlignerKey, smedja_memory::CacheAligner>>>;

/// Owns all the shared resources needed to execute a single agent turn.
pub(crate) struct TurnOrchestrator {
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn crate::embedder_port::Embedder>,
    provider_sessions: ProviderSessions,
    cache_aligners: CacheAligners,
    active_change: Option<String>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
    max_tool_turns: Option<u32>,
}

impl TurnOrchestrator {
    #[allow(clippy::too_many_arguments)] // forwarded directly from run_turn / loop runner
    pub(crate) fn new(
        ingot: IngotHandle,
        dispatcher: Arc<Dispatcher>,
        gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
        pool: Arc<ProviderPool>,
        assayer: Arc<Assayer>,
        price_table: Arc<PriceTable>,
        vault: Arc<Mutex<Vault>>,
        embedder: Arc<dyn crate::embedder_port::Embedder>,
        provider_sessions: ProviderSessions,
        cache_aligners: CacheAligners,
        active_change: Option<String>,
        lsp_manager: Arc<smedja_lsp::LspManager>,
    ) -> Self {
        Self {
            ingot,
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
            embedder,
            provider_sessions,
            cache_aligners,
            active_change,
            lsp_manager,
            max_tool_turns: None,
        }
    }

    /// Overrides the per-turn tool-call cap for loop-runner sessions.
    pub(crate) fn cap_tool_turns(mut self, n: u32) -> Self {
        self.max_tool_turns = Some(n);
        self
    }
}
