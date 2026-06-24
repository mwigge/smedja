//! `smedja-security` — the smedja security plane.
//!
//! Provides workspace posture scanning, tool-output secret scanning, and
//! CycloneDX-style SBOM assembly. Every control is **advisory by default**: a
//! finding becomes an advisory [`smedja_ingot::AuditEvent`] (`status = "warn"`)
//! and the originating action proceeds unchanged. Enforcement — promoting a
//! finding to a block or redaction — happens only when the `[security]` config
//! block sets `enforce = true`, and even then only for findings at or above
//! [`SecurityConfig::enforce_min_severity`] (which defaults to the highest
//! severity).
//!
//! The crate is synchronous and contains no async functions; callers running
//! inside a Tokio runtime should invoke the scans via
//! [`tokio::task::spawn_blocking`] so the executor thread is never blocked.

pub mod config;
pub mod finding;
pub mod output;
pub mod posture;
pub mod sbom;

pub use config::SecurityConfig;
pub use finding::{Finding, Severity};
pub use output::{scan_output, OutputScan};
pub use posture::scan_posture;
pub use sbom::{Sbom, SbomComponent};

/// Errors returned by `smedja-security` operations.
#[derive(Debug, thiserror::Error)]
pub enum SecurityError {
    /// An I/O operation failed (e.g. reading a lockfile).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// A TOML config value could not be parsed.
    #[error("config parse error: {0}")]
    Config(String),

    /// A lockfile could not be parsed into an SBOM.
    #[error("lockfile parse error: {0}")]
    Lockfile(String),
}
