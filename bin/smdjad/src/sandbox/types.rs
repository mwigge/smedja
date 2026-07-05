//! Sandbox value types: telemetry, network policy, fallback mode, and the backend trait.

use std::path::Path;

use async_trait::async_trait;

/// Structured attributes for the `smedja.sandbox.exec` span and the
/// `smedja.sandbox.unconfined` event. Built once per sandboxed execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxTelemetry {
    /// Selected backend name (`docker`/`seatbelt`/`landlock`/`none`).
    pub backend: &'static str,
    /// Active network policy.
    pub network_policy: &'static str,
    /// Active fallback mode.
    pub mode: &'static str,
    /// The confined writable root for this execution.
    pub confined_root: String,
    /// `true` when the active backend confines the command's filesystem reads
    /// to the system-dir allow-list plus the confined root.
    pub read_confined: bool,
    /// `true` when the active backend denies the command all network egress
    /// (network policy `none` with an available backend).
    pub net_confined: bool,
}

/// Declarative network policy for sandboxed commands.
///
/// Parsed from `SMEDJA_SANDBOX_NETWORK` (default [`NetworkPolicy::None`]). The
/// `allowlist`/`open` policies share the daemon's `is_blocked_ip` predicate as
/// the egress floor, so private/loopback/IMDS ranges stay blocked under every
/// policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No egress at all.
    None,
    /// Egress only to destinations not rejected by `is_blocked_ip`.
    Allowlist,
    /// General egress, but `is_blocked_ip` ranges stay blocked.
    Open,
}

impl NetworkPolicy {
    /// Parses the policy from `SMEDJA_SANDBOX_NETWORK`, defaulting to
    /// [`NetworkPolicy::None`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_str_value(&std::env::var("SMEDJA_SANDBOX_NETWORK").unwrap_or_default())
    }

    /// Parses the policy from a raw string value (case-insensitive).
    #[must_use]
    pub fn from_str_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "allowlist" => Self::Allowlist,
            "open" => Self::Open,
            _ => Self::None,
        }
    }

    /// Returns the canonical string form used in telemetry and status output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Allowlist => "allowlist",
            Self::Open => "open",
        }
    }

    /// Returns `true` when egress to a *publicly routable* destination is
    /// permitted by this policy. The `is_blocked_ip` floor is applied
    /// separately by [`NetworkPolicy::permits_dest`].
    #[must_use]
    pub fn permits_public_egress(self) -> bool {
        matches!(self, Self::Allowlist | Self::Open)
    }

    /// Returns `true` when egress to `addr` is permitted under this policy.
    ///
    /// `None` denies everything. `Allowlist`/`Open` permit only destinations
    /// not rejected by the shared `is_blocked_ip` SSRF floor, so the sandbox
    /// and the SSRF guard share one source of truth.
    #[must_use]
    pub fn permits_dest(self, addr: std::net::IpAddr) -> bool {
        match self {
            Self::None => false,
            Self::Allowlist | Self::Open => !crate::is_blocked_ip(addr),
        }
    }
}

/// Fallback behaviour when no isolation backend is available.
///
/// Parsed from `SMEDJA_SANDBOX_MODE` (default [`SandboxMode::Auto`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// Best-effort: fall back to host execution with an unconfined marker.
    Auto,
    /// Fail closed: error if no backend is available; never run unconfined.
    Required,
    /// Skip the sandbox entirely.
    Off,
}

impl SandboxMode {
    /// Parses the mode from `SMEDJA_SANDBOX_MODE`, defaulting to
    /// [`SandboxMode::Auto`].
    #[must_use]
    pub fn from_env() -> Self {
        Self::from_str_value(&std::env::var("SMEDJA_SANDBOX_MODE").unwrap_or_default())
    }

    /// Parses the mode from a raw string value (case-insensitive).
    #[must_use]
    pub fn from_str_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "required" => Self::Required,
            "off" => Self::Off,
            _ => Self::Auto,
        }
    }

    /// Returns the canonical string form used in telemetry and status output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
}

/// A platform isolation mechanism that can confine a shell command.
#[async_trait]
pub trait SandboxBackend: Send + Sync {
    /// Stable identifier reported in telemetry and `smj sandbox status`.
    fn name(&self) -> &'static str;

    /// Returns `true` when this backend can actually confine a command on the
    /// current host (binary present, kernel support, image built, …).
    fn available(&self) -> bool;

    /// Executes `cmd` confined to `confined_root` under `policy`.
    ///
    /// Returns the combined stdout on success.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a diagnostic string when the confined root is
    /// invalid, the command times out, or the command exits non-zero.
    async fn exec(
        &self,
        cmd: &str,
        confined_root: &Path,
        policy: NetworkPolicy,
    ) -> Result<String, String>;
}
