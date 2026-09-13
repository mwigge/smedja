//! GitHub Copilot CLI provider, driven over gated ACP.

use crate::{AcpProvider, COPILOT_ACP};

/// Entry point for the installed `copilot` CLI. Authentication is managed by
/// the CLI (`copilot login` or a supported token environment variable).
pub struct CopilotProvider;

impl CopilotProvider {
    /// Returns a provider only when the current Copilot CLI is installed.
    #[must_use]
    pub fn detect() -> Option<AcpProvider> {
        AcpProvider::detect(COPILOT_ACP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_matches_current_cli_availability() {
        assert_eq!(
            CopilotProvider::detect().is_some(),
            crate::SubprocessProvider::available("copilot")
        );
    }
}
