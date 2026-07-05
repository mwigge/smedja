//! Daemon bootstrap plumbing: task supervision, bounded blocking fan-out, the
//! turn worker loop, systemd readiness notification, and tracing init.

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
use crate::{embedder_port, handlers, orchestrator, run_turn};

/// Spawns a long-lived background subsystem, logging if it ever panics or exits.
///
/// The subsystem futures wrapped here (`serve` loops, GC loops, subscriber
/// loops) are meant to run for the daemon's whole lifetime, so *either* a panic
/// *or* a normal return means that subsystem silently died. A bare
/// `tokio::spawn` swallows both; this wrapper makes the death visible in the
/// logs. Behaviour is otherwise identical to `tokio::spawn`.
pub(crate) fn spawn_supervised<F>(name: &'static str, fut: F) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    use futures_util::future::FutureExt as _;
    tokio::spawn(async move {
        match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
            Ok(()) => tracing::error!(task = name, "supervised subsystem exited unexpectedly"),
            Err(_) => tracing::error!(task = name, "supervised subsystem panicked"),
        }
    })
}

/// Runs `job` on the blocking pool, but only after acquiring a permit from
/// `sem`, so at most `sem`'s permit-count jobs run concurrently. Awaiting the
/// permit provides backpressure: an unbounded fan-out of `spawn_blocking` can
/// otherwise saturate the blocking pool and starve DB writes.
pub(crate) async fn spawn_blocking_bounded<F>(sem: &Arc<tokio::sync::Semaphore>, job: F)
where
    F: FnOnce() + Send + 'static,
{
    // The semaphore is never closed, so acquire only fails if it is — treat that
    // defensively as "skip" rather than unwrapping.
    let Ok(permit) = Arc::clone(sem).acquire_owned().await else {
        return;
    };
    tokio::task::spawn_blocking(move || {
        // Held for the job's duration; dropped (releasing the permit) on return.
        let _permit = permit;
        job();
    });
}

/// Spawns the turn-worker loop: pulls `(session_id, turn_id)` jobs off `work_rx`
/// and drives each through [`run_turn`], registering an abort handle so
/// `turn.cancel` can interrupt an in-flight turn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_worker(
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn embedder_port::Embedder>,
    provider_sessions: orchestrator::ProviderSessions,
    cache_aligners: orchestrator::CacheAligners,
    mut work_rx: tokio::sync::mpsc::Receiver<(String, String)>,
    turn_registry: handlers::TurnRegistry,
    active_change: Option<Arc<str>>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
) -> tokio::task::JoinHandle<tokio::task::JoinSet<()>> {
    tokio::spawn(async move {
        let mut set = tokio::task::JoinSet::new();
        loop {
            let Some((session_id, turn_id)) = work_rx.recv().await else {
                break; // all senders dropped — daemon shutting down
            };
            let ig = ingot.clone();
            let dp = Arc::clone(&dispatcher);
            let g = Arc::clone(&gates);
            let pl = Arc::clone(&pool);
            let as_ = Arc::clone(&assayer);
            let pt = Arc::clone(&price_table);
            let vt = Arc::clone(&vault);
            let em = Arc::clone(&embedder);
            let ps = Arc::clone(&provider_sessions);
            let ca = Arc::clone(&cache_aligners);
            let reg = Arc::clone(&turn_registry);
            let ac = active_change.clone();
            let lsp = Arc::clone(&lsp_manager);
            let handle = set.spawn(run_turn(
                ig,
                dp,
                session_id,
                turn_id.clone(),
                g,
                pl,
                as_,
                pt,
                vt,
                em,
                ps,
                ca,
                Arc::clone(&turn_registry),
                ac,
                lsp,
            ));
            // Register the abort handle so `turn.cancel` can interrupt this turn.
            if let Ok(mut map) = reg.lock() {
                map.insert(turn_id, handle);
            }
            // Reap finished tasks so the set tracks only in-flight work.
            while set.try_join_next().is_some() {}
            tracing::debug!(in_flight = set.len(), "turn spawned");
        }
        set
    })
}

/// Signals `READY=1` to systemd via `$NOTIFY_SOCKET` (for `Type=notify` units),
/// after the socket is bound and the database is open. A no-op when not run
/// under systemd (the variable is absent) or off Linux.
#[cfg(target_os = "linux")]
pub(crate) fn sd_notify_ready() {
    let Ok(path) = std::env::var("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else {
        return;
    };
    let sent = if let Some(name) = path.strip_prefix('@') {
        // Abstract namespace socket (common for user services).
        use std::os::linux::net::SocketAddrExt as _;
        std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())
            .and_then(|addr| sock.send_to_addr(b"READY=1", &addr))
            .is_ok()
    } else {
        sock.send_to(b"READY=1", &path).is_ok()
    };
    if sent {
        tracing::debug!("notified systemd: READY=1");
    }
}

/// No-op readiness notification off Linux (systemd is Linux-only).
#[cfg(not(target_os = "linux"))]
pub(crate) fn sd_notify_ready() {}

/// Initialises the tracing subscriber, honouring `SMEDJA_LOG_FORMAT`.
///
/// `text` (default) uses the human-readable formatter; `json` emits structured
/// JSON for log-ingestion pipelines (Loki, `OpenSearch`); an unrecognised value
/// falls back to text with a warning.
pub(crate) fn init_tracing() {
    match std::env::var("SMEDJA_LOG_FORMAT").as_deref() {
        Ok("json") => tracing_subscriber::fmt().json().init(),
        Ok("text" | "") | Err(_) => tracing_subscriber::fmt().init(),
        Ok(other) => {
            tracing_subscriber::fmt().init();
            tracing::warn!(format = other, "unrecognised SMEDJA_LOG_FORMAT; using text");
        }
    }
}
