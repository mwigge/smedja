//! The provider-backed [`ReviewTurn`] that drives the real Review-role provider.

use std::sync::Arc;

use smedja_adapter::types::Message as AdapterMessage;
use smedja_adapter::CallOptions;
use smedja_assayer::{Runner, Tier};
use smedja_bellows::event::CorrelationCtx;
use smedja_bellows::Dispatcher;
use smedja_rpc::{codes, RpcError};

use super::review_loop::{ReviewTurn, TurnOutput};
use crate::provider_pool::ProviderPool;

/// Provider-backed [`ReviewTurn`] that drives the real Review-role provider.
pub(crate) struct ProviderReviewTurn {
    pub(crate) pool: Arc<ProviderPool>,
    pub(crate) dispatcher: Arc<Dispatcher>,
    pub(crate) model_override: Option<String>,
}

impl ReviewTurn for ProviderReviewTurn {
    async fn run_turn(&self, transcript: &[AdapterMessage]) -> Result<TurnOutput, RpcError> {
        // The Review role routes to a deep provider; rotate over the eligible
        // ring until one serves the turn.
        let ring = self.pool.eligible_ring(Runner::Claude, Tier::Deep);
        if ring.is_empty() {
            return Err(RpcError::new(
                codes::INTERNAL_ERROR,
                "no LLM provider available for the review role",
            ));
        }

        // Split the system prompt out of the transcript for CallOptions.
        let system = transcript
            .iter()
            .find(|m| matches!(m.role, smedja_adapter::types::Role::System))
            .map(|m| m.content.clone());
        let body: Vec<AdapterMessage> = transcript
            .iter()
            .filter(|m| !matches!(m.role, smedja_adapter::types::Role::System))
            .cloned()
            .collect();

        let mut last_err = String::from("no provider attempt");
        for entry in ring {
            let model = self
                .model_override
                .clone()
                .or_else(|| std::env::var("SMEDJA_MODEL").ok())
                .unwrap_or_else(|| entry.default_model.clone());
            let opts = CallOptions {
                model,
                max_tokens: Some(2048),
                temperature: Some(0.2),
                system: system.clone(),
                tools: None,
                provider_session_id: None,
                smedja_session_id: None,
                permission_mode: None,
                stable_prefix_len: None,
                cache_strategy: smedja_adapter::CacheStrategy::None,
                workspace: None,
            };
            let stream = entry.provider.stream_chat(&body, &opts);
            let drained = tokio::time::timeout(
                std::time::Duration::from_mins(5),
                crate::common::drain_stream(
                    stream,
                    &self.dispatcher,
                    None,
                    &CorrelationCtx::default(),
                ),
            )
            .await;
            match drained {
                Ok(Ok((text, input_tokens, output_tokens, _cache_read, _session))) => {
                    return Ok(TurnOutput {
                        text,
                        input_tokens: u64::from(input_tokens),
                        output_tokens: u64::from(output_tokens),
                    });
                }
                Ok(Err(e)) => last_err = e.to_string(),
                Err(_) => "review turn timed out after 300s".clone_into(&mut last_err),
            }
        }
        Err(RpcError::new(
            codes::INTERNAL_ERROR,
            format!("review turn failed: {last_err}"),
        ))
    }
}
