//! The [`SandboxExecutor`] that dispatches shell/tool execution to the selected
//! backend and applies the no-backend fallback contract.

use std::path::Path;

use which::which;

use super::backend::{native_backend, select_backend};
use super::{DockerBackend, NetworkPolicy, SandboxBackend, SandboxMode, SandboxTelemetry};

/// Tools exempt from sandboxing (read-only; no side-effects).
const EXEMPT_TOOLS: &[&str] = &["read_file", "list_files", "graph_query"];

/// Marker prefixed to a tool result that ran on the host without confinement.
pub const UNCONFINED_MARKER: &str = "[unconfined] sandbox unavailable; command ran on the host\n";

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

#[cfg(test)]
mod tests {
    use super::{SandboxExecutor, UNCONFINED_MARKER};
    use crate::sandbox::backend::StubBackend;
    use crate::sandbox::{NetworkPolicy, SandboxMode};
    use std::path::Path;

    // ── 1.1 trait dispatch ────────────────────────────────────────────────────

    #[tokio::test]
    async fn backend_trait_dispatches_to_selected_impl() {
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Auto,
            network: NetworkPolicy::None,
        };
        assert!(ex.available());
        assert_eq!(ex.backend_name(), "stub");
        let out = ex.exec("echo hi", Path::new("/tmp")).await.unwrap();
        assert!(out.starts_with("stub:echo hi:"), "got: {out}");
        assert!(
            out.ends_with(":none"),
            "policy must be threaded; got: {out}"
        );
    }

    #[test]
    fn read_file_is_exempt() {
        assert!(SandboxExecutor::is_exempt("read_file"));
    }

    #[test]
    fn bash_is_not_exempt() {
        assert!(!SandboxExecutor::is_exempt("bash"));
    }

    #[test]
    fn mcp_call_is_not_exempt() {
        assert!(!SandboxExecutor::is_exempt("mcp_call"));
    }

    #[tokio::test]
    async fn exec_unavailable_returns_err() {
        let ex = SandboxExecutor {
            backend: None,
            mode: SandboxMode::Auto,
            network: NetworkPolicy::None,
        };
        assert!(!ex.available());
        assert!(ex.exec("ls", Path::new("/tmp")).await.is_err());
    }

    // ── 6.1 fallback contract ─────────────────────────────────────────────────

    fn no_backend(mode: SandboxMode) -> SandboxExecutor {
        SandboxExecutor {
            backend: None,
            mode,
            network: NetworkPolicy::None,
        }
    }

    #[tokio::test]
    async fn required_fails_closed() {
        let ex = no_backend(SandboxMode::Required);
        let mut ran = false;
        let out = ex
            .run_confined("echo hi", Path::new("/tmp"), || {
                ran = true;
                async { "host-output".to_owned() }
            })
            .await;
        assert!(
            out.starts_with("error:"),
            "required must fail closed; got: {out}"
        );
        assert!(
            out.contains("no isolation backend"),
            "must name the missing capability; got: {out}"
        );
        assert!(!ran, "required must NOT execute the command");
    }

    #[tokio::test]
    async fn auto_falls_back_with_marker() {
        let ex = no_backend(SandboxMode::Auto);
        let out = ex
            .run_confined("echo hi", Path::new("/tmp"), || async {
                "host-output".to_owned()
            })
            .await;
        assert!(
            out.starts_with(UNCONFINED_MARKER),
            "auto must stamp the marker; got: {out}"
        );
        assert!(
            out.contains("host-output"),
            "auto must run on the host; got: {out}"
        );
    }

    #[tokio::test]
    async fn off_skips_sandbox() {
        let ex = no_backend(SandboxMode::Off);
        let out = ex
            .run_confined("echo hi", Path::new("/tmp"), || async {
                "host-output".to_owned()
            })
            .await;
        assert_eq!(
            out, "host-output",
            "off must run on the host with no marker"
        );
    }

    // ── 7.1 telemetry attributes ──────────────────────────────────────────────

    #[test]
    fn sandbox_exec_emits_span_with_backend_attributes() {
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Required,
            network: NetworkPolicy::Allowlist,
        };
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert_eq!(tel.backend, "stub");
        assert_eq!(tel.network_policy, "allowlist");
        assert_eq!(tel.mode, "required");
        assert_eq!(tel.confined_root, "/tmp/wt");

        // No backend → telemetry records "none".
        let ex = no_backend(SandboxMode::Auto);
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert_eq!(tel.backend, "none");
    }

    // ── 1.3 telemetry records read/net confinement ────────────────────────────

    #[test]
    fn telemetry_records_read_and_net_confinement() {
        // A backend-backed executor under network=none reports both confinements.
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Auto,
            network: NetworkPolicy::None,
        };
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert!(tel.read_confined, "active backend confines reads");
        assert!(
            tel.net_confined,
            "network=none with an active backend confines the network"
        );

        // No backend → neither confinement applies.
        let ex = no_backend(SandboxMode::Auto);
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert!(!tel.read_confined, "no backend → reads not confined");
        assert!(!tel.net_confined, "no backend → network not confined");

        // open network with a backend → reads confined, network not confined.
        let ex = SandboxExecutor {
            backend: Some(Box::new(StubBackend {
                name: "stub",
                avail: true,
            })),
            mode: SandboxMode::Auto,
            network: NetworkPolicy::Open,
        };
        let tel = ex.telemetry(Path::new("/tmp/wt"));
        assert!(tel.read_confined);
        assert!(!tel.net_confined, "open egress is not a confined network");
    }
}
