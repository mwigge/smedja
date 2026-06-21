//! `smedja-sre` — async HTTP query tools for `SigNoz`, Prometheus, and Loki.
//!
//! Three free functions cover the three observability pillars:
//!
//! - [`otel_query`] — traces (`SigNoz`)
//! - [`metric_query`] — metrics (Prometheus)
//! - [`log_tail`] — logs (Loki)
//!
//! All functions return `serde_json::Value` so callers can forward raw API
//! responses to an LLM without imposing a rigid schema.

mod config;
mod error;
mod logs;
mod metrics;
mod otel;

pub use config::SreConfig;
pub use error::SreError;
pub use logs::log_tail;
pub use metrics::metric_query;
pub use otel::otel_query;
