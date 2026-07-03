//! The [`SandboxBackend`] trait and backend selection logic.
//!
//! [`select_backend`] applies the Docker → OS-native → none precedence and
//! [`native_backend`] constructs the current platform's OS-native backend.

use std::path::Path;

use async_trait::async_trait;

#[cfg(target_os = "linux")]
use super::LandlockBackend;
use super::NetworkPolicy;
#[cfg(target_os = "macos")]
use super::SeatbeltBackend;

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

/// Selects the active backend from the available implementations.
///
/// Precedence: Docker when opted in and reachable, else the current platform's
/// OS-native backend (when available), else `None`. `docker_opt_in` reflects
/// whether the operator pinned Docker (legacy `SMEDJA_TOOL_SANDBOX=docker` or
/// an explicit selection).
pub(crate) fn select_backend(
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
pub(crate) fn native_backend() -> Option<Box<dyn SandboxBackend>> {
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

/// A stub backend that records its inputs and returns a canned string. Shared
/// by the sandbox test modules to exercise selection and dispatch without a
/// real isolation mechanism.
#[cfg(test)]
pub(crate) struct StubBackend {
    pub(crate) name: &'static str,
    pub(crate) avail: bool,
}

#[cfg(test)]
#[async_trait]
impl SandboxBackend for StubBackend {
    fn name(&self) -> &'static str {
        self.name
    }
    fn available(&self) -> bool {
        self.avail
    }
    async fn exec(
        &self,
        cmd: &str,
        confined_root: &Path,
        policy: NetworkPolicy,
    ) -> Result<String, String> {
        Ok(format!(
            "stub:{}:{}:{}",
            cmd,
            confined_root.display(),
            policy.as_str()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{select_backend, SandboxBackend, StubBackend};

    // ── 1.2 selection precedence ──────────────────────────────────────────────

    #[test]
    fn selection_prefers_docker_then_native_then_none() {
        let docker_avail = || -> Box<dyn SandboxBackend> {
            Box::new(StubBackend {
                name: "docker",
                avail: true,
            })
        };
        let docker_unavail = || -> Box<dyn SandboxBackend> {
            Box::new(StubBackend {
                name: "docker",
                avail: false,
            })
        };
        let native_avail = || -> Box<dyn SandboxBackend> {
            Box::new(StubBackend {
                name: "native",
                avail: true,
            })
        };

        // Docker opted in and available → Docker wins.
        let sel = select_backend(true, docker_avail(), Some(native_avail()));
        assert_eq!(sel.unwrap().name(), "docker");

        // Docker opted in but unavailable → native wins.
        let sel = select_backend(true, docker_unavail(), Some(native_avail()));
        assert_eq!(sel.unwrap().name(), "native");

        // Docker not opted in → native wins even if docker is available.
        let sel = select_backend(false, docker_avail(), Some(native_avail()));
        assert_eq!(sel.unwrap().name(), "native");

        // No native available → none.
        let sel = select_backend(true, docker_unavail(), None);
        assert!(sel.is_none());
    }
}
