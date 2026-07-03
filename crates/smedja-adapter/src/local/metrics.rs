//! `OTel` counters for the local adapter and the public swap-metric recorder.

use std::sync::OnceLock;

use opentelemetry::metrics::Counter;

/// Returns (initialising on first call) the `smedja_local_health_checks_total` counter.
pub(crate) fn health_check_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("smedja-adapter")
            .u64_counter("smedja_local_health_checks_total")
            .with_description(
                "Total number of local endpoint health checks, labelled by result (ok|error).",
            )
            .build()
    })
}

/// Returns (initialising on first call) the `smedja_local_swaps_total` counter.
fn swap_counter() -> &'static Counter<u64> {
    static COUNTER: OnceLock<Counter<u64>> = OnceLock::new();
    COUNTER.get_or_init(|| {
        opentelemetry::global::meter("smedja-adapter")
            .u64_counter("smedja_local_swaps_total")
            .with_description("Total number of local model swaps, labelled by result (ok|error).")
            .build()
    })
}

/// Records a local-model swap result on the `smedja_local_swaps_total` counter,
/// labelled `result = ok | error`.
///
/// Exposed so the daemon's `local.swap` handler — which issues the swap directly
/// via [`crate::issue_swap_request`] rather than [`crate::LocalProvider::swap_model`] —
/// records the same metric on the same instrument.
pub fn record_local_swap(ok: bool) {
    let result = if ok { "ok" } else { "error" };
    swap_counter().add(1, &[opentelemetry::KeyValue::new("result", result)]);
}
