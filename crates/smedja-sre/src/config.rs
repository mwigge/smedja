use crate::SreError;

/// Runtime configuration for SRE query endpoints.
///
/// Load from environment variables via [`SreConfig::from_env`] or construct
/// directly with [`SreConfig::new`] for tests and programmatic use.
#[derive(Debug, Clone)]
pub struct SreConfig {
    /// Base URL for `SigNoz` (e.g. `http://localhost:3301`).
    pub otlp_endpoint: String,
    /// Base URL for Prometheus (e.g. `http://localhost:9090`).
    pub prometheus_endpoint: String,
    /// Base URL for Loki (e.g. `http://localhost:3100`).
    pub loki_endpoint: String,
}

impl SreConfig {
    /// Reads configuration from environment variables.
    ///
    /// # Errors
    ///
    /// Returns [`SreError::MissingEnvVar`] if any of the following variables
    /// are absent or contain invalid Unicode:
    ///
    /// - `SMEDJA_OTLP_ENDPOINT`
    /// - `SMEDJA_PROMETHEUS_ENDPOINT`
    /// - `SMEDJA_LOKI_ENDPOINT`
    #[must_use = "the returned config must be used to issue queries"]
    pub fn from_env() -> Result<Self, SreError> {
        let otlp_endpoint =
            std::env::var("SMEDJA_OTLP_ENDPOINT").map_err(|e| SreError::MissingEnvVar {
                var: "SMEDJA_OTLP_ENDPOINT",
                source: e,
            })?;
        let prometheus_endpoint =
            std::env::var("SMEDJA_PROMETHEUS_ENDPOINT").map_err(|e| SreError::MissingEnvVar {
                var: "SMEDJA_PROMETHEUS_ENDPOINT",
                source: e,
            })?;
        let loki_endpoint =
            std::env::var("SMEDJA_LOKI_ENDPOINT").map_err(|e| SreError::MissingEnvVar {
                var: "SMEDJA_LOKI_ENDPOINT",
                source: e,
            })?;
        Ok(Self {
            otlp_endpoint,
            prometheus_endpoint,
            loki_endpoint,
        })
    }

    /// Constructs a config with explicit values (for tests or programmatic use).
    #[must_use]
    pub fn new(
        otlp: impl Into<String>,
        prometheus: impl Into<String>,
        loki: impl Into<String>,
    ) -> Self {
        Self {
            otlp_endpoint: otlp.into(),
            prometheus_endpoint: prometheus.into(),
            loki_endpoint: loki.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn config_from_env_reads_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("SMEDJA_OTLP_ENDPOINT", "http://otlp:3301");
        std::env::set_var("SMEDJA_PROMETHEUS_ENDPOINT", "http://prom:9090");
        std::env::set_var("SMEDJA_LOKI_ENDPOINT", "http://loki:3100");

        let cfg = SreConfig::from_env().expect("from_env should succeed when all vars are set");

        assert_eq!(cfg.otlp_endpoint, "http://otlp:3301");
        assert_eq!(cfg.prometheus_endpoint, "http://prom:9090");
        assert_eq!(cfg.loki_endpoint, "http://loki:3100");
    }

    #[test]
    fn config_from_env_missing_var_returns_err() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("SMEDJA_OTLP_ENDPOINT");
        std::env::remove_var("SMEDJA_PROMETHEUS_ENDPOINT");
        std::env::remove_var("SMEDJA_LOKI_ENDPOINT");

        let err = SreConfig::from_env().expect_err("from_env should fail when vars are absent");

        assert!(
            matches!(
                err,
                SreError::MissingEnvVar {
                    var: "SMEDJA_OTLP_ENDPOINT",
                    ..
                }
            ),
            "unexpected error variant: {err}"
        );
    }
}
