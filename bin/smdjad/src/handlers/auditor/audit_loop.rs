//! The read-only exploration loop: budget/turn types, the `ReviewTurn`
//! abstraction with its live provider implementation, and the loop driver.
//! Moved verbatim from `auditor.rs`.

use super::*;

/// Default upper bound on exploration iterations for one audit run.
pub(crate) const DEFAULT_MAX_ITERATIONS: u32 = 12;

/// Default token budget for one audit run (input + output, summed across turns).
pub(crate) const DEFAULT_TOKEN_BUDGET: u64 = 200_000;

// ── Read-only exploration loop ───────────────────────────────────────────────

/// Drives one review-role turn, returning the model's text output.
///
/// Abstracted as a trait so the loop can be exercised with a deterministic mock
/// in tests without a live provider.
pub(crate) trait ReviewTurn: Send + Sync {
    /// Runs one turn given the running transcript, returning the model text.
    fn run_turn(
        &self,
        transcript: &[AdapterMessage],
    ) -> impl std::future::Future<Output = Result<TurnOutput, RpcError>> + Send;
}

/// The result of one review turn: the model text and its token usage.
pub(crate) struct TurnOutput {
    /// The model's full text response for the turn.
    pub(crate) text: String,
    /// Input tokens consumed by the turn.
    pub(crate) input_tokens: u64,
    /// Output tokens produced by the turn.
    pub(crate) output_tokens: u64,
}

/// Bounds for one audit loop.
pub(crate) struct LoopBudget {
    /// Hard cap on exploration iterations.
    pub(crate) max_iterations: u32,
    /// Cap on summed input+output tokens.
    pub(crate) token_budget: u64,
}

impl Default for LoopBudget {
    fn default() -> Self {
        Self {
            max_iterations: DEFAULT_MAX_ITERATIONS,
            token_budget: DEFAULT_TOKEN_BUDGET,
        }
    }
}

/// The system prompt steering the read-only auditor.
fn audit_system_prompt() -> String {
    format!(
        "You are a meticulous, read-only code auditor. Explore the codebase using \
         ONLY these tools: {tools}. You MUST NOT attempt to modify any file or run \
         any write command. To call a tool, emit a single JSON object \
         {{\"tool\": <name>, \"input\": {{...}}}}. When you have gathered enough \
         context, emit your findings as a fenced JSON array of objects with the \
         fields severity (critical|high|medium|low|info), file, line (optional \
         integer), rule (short slug), and rationale (one sentence). Emit the \
         findings array and stop.",
        tools = AUDIT_TOOLS.join(", ")
    )
}

/// Runs the bounded, read-only exploration loop.
///
/// Seed → review turn → optional allowed tool call (rejected if outside the
/// allowlist) → append observation → repeat, bounded by `budget`. Returns the
/// final de-duplicated findings.
///
/// The loop only ever dispatches tools in [`AUDIT_TOOLS`]; any other tool call
/// is rejected and fed back as an error observation, so no write tool is ever
/// constructed.
///
/// # Errors
///
/// Returns an [`RpcError`] when a review turn fails.
#[allow(clippy::too_many_arguments)] // forwards the read-only audit tool-loop dependencies
pub(crate) async fn run_audit_loop<R: ReviewTurn>(
    runner: &R,
    seed: &str,
    workspace: &Path,
    session: &Session,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
    embedder: &Arc<dyn crate::embedder_port::Embedder>,
    budget: &LoopBudget,
) -> Result<Vec<AuditFinding>, RpcError> {
    debug_assert_eq!(
        session.mode.as_deref(),
        Some("review"),
        "audit loop must run in review mode"
    );

    let mut transcript = vec![
        AdapterMessage::system(audit_system_prompt()),
        AdapterMessage::user(format!("Audit the following scope.\n\n{seed}")),
    ];
    let mut spent_tokens = 0u64;
    let mut findings = Vec::new();

    for _iteration in 0..budget.max_iterations {
        if spent_tokens >= budget.token_budget {
            break;
        }
        let output = runner.run_turn(&transcript).await?;
        spent_tokens = spent_tokens.saturating_add(output.input_tokens);
        spent_tokens = spent_tokens.saturating_add(output.output_tokens);
        let response = output.text;

        // Any parseable findings array terminates the loop.
        let parsed = parse_findings(&response);
        if !parsed.is_empty() {
            findings = parsed;
            break;
        }

        transcript.push(AdapterMessage::assistant(response.clone()));

        let Some((tool_name, tool_input)) = crate::executor::parse_tool_call(&response) else {
            // No tool call and no findings: nothing more to explore.
            break;
        };

        // Read-only allowlist: reject anything outside AUDIT_TOOLS without
        // executing it, and feed the rejection back as an observation. A write
        // tool dispatch is never constructed.
        let observation = if is_audit_tool(&tool_name) {
            execute_tool(
                &tool_name,
                &tool_input,
                workspace,
                Some(session),
                ingot,
                vault,
                embedder,
            )
            .await
        } else {
            format!(
                "error: tool '{tool_name}' is not permitted in a read-only audit; \
                 allowed tools are {}",
                AUDIT_TOOLS.join(", ")
            )
        };
        transcript.push(AdapterMessage::user(format!("Observation:\n{observation}")));
    }

    Ok(dedup_findings(findings))
}

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
