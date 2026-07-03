//! Time-tiered metrics rollups over the cost ledger and audit log.
//!
//! Aggregates tokens, cost, turns, and error counts **per runner** into one of
//! five fixed time tiers (`raw` / `hourly` / `daily` / `weekly` / `monthly`).
//! Tokens, cost, and turns come from `cost_ledger`; error counts come from
//! `audit_events` rows with `status = 'error'`. The two grouped result sets are
//! merged in Rust on `(bucket_start, runner)` so a runner that errored without a
//! cost row — or spent without erroring — still appears.
//!
//! Aggregation is computed on read from source rows by default; an optional
//! idempotent [`materialise`] upserts the same computed buckets into the
//! `metrics_rollups` table for callers that want pre-aggregated reads. The table
//! is a derived cache, never a second source of truth.

mod compute;
mod tier;

pub use compute::MetricsBucket;
pub(crate) use compute::{compute, materialise};
pub use tier::RollupTier;
