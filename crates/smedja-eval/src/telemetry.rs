//! `OpenTelemetry` instrumentation for the eval harness.
//!
//! Mirrors the `smedja-loop` telemetry conventions: metrics are created from
//! the global meter `smedja.eval` and named `smedja_eval_*`. Every metric
//! carries a `suite` attribute so pass-rate trends are queryable per suite.

use opentelemetry::{global, KeyValue};

/// Records the suite case-count, pass-count, and derived pass-rate metrics.
///
/// Metrics recorded (each labelled `suite`):
/// - `smedja_eval_cases_total`
/// - `smedja_eval_pass_total`
/// - `smedja_eval_pass_rate`
pub fn record_eval_metrics(suite: &str, cases: u64, passed: u64) {
    let meter = global::meter("smedja.eval");
    let labels = [KeyValue::new("suite", suite.to_owned())];
    meter
        .u64_counter("smedja_eval_cases_total")
        .build()
        .add(cases, &labels);
    meter
        .u64_counter("smedja_eval_pass_total")
        .build()
        .add(passed, &labels);
    let pass_rate = if cases == 0 {
        1.0
    } else {
        // Counts are small; the f64 conversion is exact for any realistic suite.
        let passed = u32::try_from(passed).unwrap_or(u32::MAX);
        let cases = u32::try_from(cases).unwrap_or(u32::MAX);
        f64::from(passed) / f64::from(cases)
    };
    meter
        .f64_gauge("smedja_eval_pass_rate")
        .build()
        .record(pass_rate, &labels);
}

/// Records the per-suite duration histogram `smedja_eval_duration_seconds`.
pub fn record_eval_duration(suite: &str, secs: f64) {
    let meter = global::meter("smedja.eval");
    let labels = [KeyValue::new("suite", suite.to_owned())];
    meter
        .f64_histogram("smedja_eval_duration_seconds")
        .build()
        .record(secs, &labels);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_eval_metrics_runs_under_noop_meter() {
        // With no meter provider installed the global meter is a no-op; the
        // calls must not panic. Mirrors the smedja-loop telemetry tests.
        record_eval_metrics("routing", 10, 9);
        record_eval_metrics("empty", 0, 0);
    }

    #[test]
    fn record_eval_duration_runs_under_noop_meter() {
        record_eval_duration("routing", 0.42);
    }
}
