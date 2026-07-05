//! Cross-platform sandbox executor for tool isolation.
//!
//! Shell/tool execution (`bash`, `run_command`) is confined behind a
//! [`SandboxBackend`] selected by capability detection:
//!
//! - [`DockerBackend`] — opt-in (`SMEDJA_TOOL_SANDBOX=docker` or
//!   `SMEDJA_SANDBOX_MODE`/Docker reachable); strongest isolation.
//! - [`SeatbeltBackend`] — macOS `sandbox-exec`; zero-config default on macOS.
//! - [`LandlockBackend`] — Linux Landlock LSM; zero-config default on Linux.
//!
//! Selection precedence: Docker (when opted in and reachable) → the current
//! platform's OS-native backend → none. Read-only tools (`read_file`,
//! `list_files`, `graph_query`) bypass the sandbox entirely.
//!
//! The writable filesystem root is the *confined root* — the active worktree
//! when a task owns one, otherwise the session workspace — with `.git`
//! read-only and an ephemeral `/tmp`. A declarative [`NetworkPolicy`] governs
//! egress and shares the daemon's `is_blocked_ip` SSRF floor. A
//! [`SandboxMode`] governs the fallback when no backend is available.

use std::path::Path;

use which::which;

mod docker;
#[cfg(target_os = "linux")]
mod landlock_backend;
mod paths;
#[cfg(target_os = "macos")]
mod seatbelt;
#[cfg(test)]
mod tests;
mod types;

pub use docker::DockerBackend;
#[cfg(target_os = "linux")]
pub use landlock_backend::LandlockBackend;
#[cfg(target_os = "macos")]
pub use seatbelt::SeatbeltBackend;

pub use types::{NetworkPolicy, SandboxBackend, SandboxMode, SandboxTelemetry};

#[cfg(test)]
pub(crate) use paths::DEFAULT_READ_PATHS;
pub(crate) use paths::{resolve_confined_root, resolve_read_paths};

/// Tools exempt from sandboxing (read-only; no side-effects).
const EXEMPT_TOOLS: &[&str] = &["read_file", "list_files", "graph_query"];

/// Per-command execution timeout for every backend.
const EXEC_TIMEOUT_SECS: u64 = 30;

/// Marker prefixed to a tool result that ran on the host without confinement.
pub const UNCONFINED_MARKER: &str = "[unconfined] sandbox unavailable; command ran on the host\n";

/// Selects the active backend from the available implementations.
///
/// Precedence: Docker when opted in and reachable, else the current platform's
/// OS-native backend (when available), else `None`. `docker_opt_in` reflects
/// whether the operator pinned Docker (legacy `SMEDJA_TOOL_SANDBOX=docker` or
/// an explicit selection).
fn select_backend(
    docker_opt_in: bool,
    docker: Box<dyn SandboxBackend>,
    native: Option<Box<dyn SandboxBackend>>,
) -> Option<Box<dyn SandboxBackend>> {
    if docker_opt_in && docker.available() {
        return Some(docker);
    }
    match native {
        Some(n) if n.available() => Some(n),
        _ => None,
    }
}

/// Constructs the OS-native backend for the current platform, if any.
// The return type is `Option` because the unsupported-platform arm yields
// `None`; per-platform cfg makes some arms always-`Some`, which clippy can't see.
#[cfg_attr(
    any(target_os = "macos", target_os = "linux"),
    allow(clippy::unnecessary_wraps)
)]
fn native_backend() -> Option<Box<dyn SandboxBackend>> {
    #[cfg(target_os = "macos")]
    {
        Some(Box::new(SeatbeltBackend::detect()))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Box::new(LandlockBackend::detect()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        None
    }
}

/// Dispatches shell/tool execution to the selected [`SandboxBackend`].
pub struct SandboxExecutor {
    backend: Option<Box<dyn SandboxBackend>>,
    mode: SandboxMode,
    network: NetworkPolicy,
}

impl SandboxExecutor {
    /// Creates an executor, selecting the backend by capability detection.
    ///
    /// The legacy `SMEDJA_TOOL_SANDBOX=docker` alias pins Docker and sets mode
    /// `Auto`. Otherwise the mode comes from `SMEDJA_SANDBOX_MODE` and the
    /// network policy from `SMEDJA_SANDBOX_NETWORK`.
    ///
    /// # Note
    ///
    /// Capability detection (`new()`) runs once at daemon startup, before the
    /// Tokio runtime is accepting work, so brief blocking probes (`which`,
    /// `docker image inspect`) are acceptable here.
    #[must_use]
    pub fn new() -> Self {
        let legacy_docker = std::env::var("SMEDJA_TOOL_SANDBOX").is_ok_and(|v| v == "docker");
        let mode = if legacy_docker {
            SandboxMode::Auto
        } else {
            SandboxMode::from_env()
        };
        let network = NetworkPolicy::from_env();

        if matches!(mode, SandboxMode::Off) {
            return Self {
                backend: None,
                mode,
                network,
            };
        }

        // Docker is opted into via the legacy alias, or implicitly considered
        // when its daemon is reachable on PATH.
        let docker_opt_in = legacy_docker || which("docker").is_ok();
        let docker: Box<dyn SandboxBackend> = Box::new(DockerBackend::detect());
        let backend = select_backend(docker_opt_in, docker, native_backend());

        Self {
            backend,
            mode,
            network,
        }
    }

    /// Returns `true` when a usable backend was selected.
    #[must_use]
    pub fn available(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.available())
    }

    /// Returns the selected backend's name, or `"none"` when none is available.
    #[must_use]
    pub fn backend_name(&self) -> &'static str {
        self.backend.as_ref().map_or("none", |b| b.name())
    }

    /// Returns the active network policy.
    #[must_use]
    pub fn network_policy(&self) -> NetworkPolicy {
        self.network
    }

    /// Returns the active fallback mode.
    #[must_use]
    pub fn mode(&self) -> SandboxMode {
        self.mode
    }

    /// Returns `true` if `tool_name` is exempt from sandboxing.
    #[must_use]
    pub fn is_exempt(tool_name: &str) -> bool {
        EXEMPT_TOOLS.contains(&tool_name)
    }

    /// Executes `cmd` confined to `confined_root` via the selected backend.
    ///
    /// # Errors
    ///
    /// Returns `Err` when no backend is available or the backend fails.
    pub async fn exec(&self, cmd: &str, confined_root: &Path) -> Result<String, String> {
        match self.backend.as_ref() {
            Some(b) => b.exec(cmd, confined_root, self.network).await,
            None => Err("sandbox not available".into()),
        }
    }

    /// Builds the structured telemetry attributes for an execution rooted at
    /// `confined_root`.
    #[must_use]
    pub fn telemetry(&self, confined_root: &Path) -> SandboxTelemetry {
        // Read confinement applies whenever a backend is active (every backend
        // tightens reads to the allow-list / structural floor). Network
        // confinement applies only under policy `none`, the sole policy that
        // denies the subprocess all egress; `allowlist`/`open` retain host
        // network (open-minus-blocked-ranges for the subprocess).
        let active = self.available();
        let read_confined = active;
        let net_confined = active && matches!(self.network, NetworkPolicy::None);

        SandboxTelemetry {
            backend: self.backend_name(),
            network_policy: self.network.as_str(),
            mode: self.mode.as_str(),
            confined_root: confined_root.display().to_string(),
            read_confined,
            net_confined,
        }
    }

    /// Runs `cmd` confined to `confined_root`, applying the fallback contract
    /// and emitting the `smedja.sandbox.exec` span and (on host fallback) the
    /// `smedja.sandbox.unconfined` event. `host_run` is the unconfined host
    /// runner used by the `Auto` fallback.
    ///
    /// Returns the tool-result string (an `error:`-form string under
    /// `Required` with no backend; an [`UNCONFINED_MARKER`]-prefixed result
    /// under `Auto` with no backend; the backend output otherwise).
    pub async fn run_confined<F, Fut>(&self, cmd: &str, confined_root: &Path, host_run: F) -> String
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = String>,
    {
        let tel = self.telemetry(confined_root);
        let span = tracing::info_span!(
            "smedja.sandbox.exec",
            backend = tel.backend,
            network_policy = tel.network_policy,
            mode = tel.mode,
            confined_root = %tel.confined_root,
            read_confined = tel.read_confined,
            net_confined = tel.net_confined,
        );
        let _enter = span.enter();

        if self.available() {
            return match self.exec(cmd, confined_root).await {
                Ok(out) => out,
                Err(e) => format!("error: {e}"),
            };
        }

        // No backend available: apply the mode-driven fallback.
        match self.mode {
            SandboxMode::Off => host_run().await,
            SandboxMode::Required => {
                format!(
                    "error: sandbox required but no isolation backend is available \
                     (mode=required, network={}); install Docker or enable a supported \
                     OS-native backend, or set SMEDJA_SANDBOX_MODE=auto",
                    tel.network_policy
                )
            }
            SandboxMode::Auto => {
                tracing::info!(
                    backend = tel.backend,
                    network_policy = tel.network_policy,
                    mode = tel.mode,
                    confined_root = %tel.confined_root,
                    reason = "no isolation backend available",
                    "smedja.sandbox.unconfined"
                );
                let out = host_run().await;
                format!("{UNCONFINED_MARKER}{out}")
            }
        }
    }
}

impl Default for SandboxExecutor {
    fn default() -> Self {
        Self::new()
    }
}
