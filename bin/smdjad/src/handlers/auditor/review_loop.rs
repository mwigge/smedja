//! The bounded, read-only exploration loop and its [`ReviewTurn`] abstraction.
//!
//! The loop is genuinely read-only: it only ever dispatches tools in
//! [`AUDIT_TOOLS`]; any other tool call is rejected and fed back as an error
//! observation, so no write tool is ever constructed.

use std::path::Path;
use std::sync::Arc;

use smedja_adapter::types::Message as AdapterMessage;
use smedja_ingot::{IngotHandle, Session};
use smedja_rpc::RpcError;
use smedja_vault::Vault;
use tokio::sync::Mutex;

use super::findings::{dedup_findings, parse_findings, AuditFinding};
use super::{is_audit_tool, AUDIT_TOOLS, DEFAULT_MAX_ITERATIONS, DEFAULT_TOKEN_BUDGET};
use crate::executor::execute_tool;

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

#[cfg(test)]
mod tests {
    use super::super::findings::render_report;
    use super::super::persist::persist_findings;
    use super::*;
    use smedja_types::Timestamp;
    use uuid::Uuid;

    fn ingot() -> IngotHandle {
        IngotHandle::new(smedja_ingot::Ingot::open_in_memory().unwrap())
    }

    fn vault() -> Arc<Mutex<Vault>> {
        Arc::new(Mutex::new(Vault::open_in_memory().unwrap()))
    }

    fn embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    fn review_session() -> Session {
        Session {
            id: Uuid::new_v4(),
            created_at: Timestamp::from_micros(0),
            updated_at: Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: Some("review".to_owned()),
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    /// A scripted runner that returns each response in order, recording the
    /// observations it was fed back so a test can assert on rejection text.
    struct ScriptedRunner {
        responses: std::sync::Mutex<std::collections::VecDeque<String>>,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedRunner {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: std::sync::Mutex::new(
                    responses.into_iter().map(ToOwned::to_owned).collect(),
                ),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ReviewTurn for ScriptedRunner {
        async fn run_turn(&self, transcript: &[AdapterMessage]) -> Result<TurnOutput, RpcError> {
            if let Some(last) = transcript.last() {
                self.seen.lock().unwrap().push(last.content.clone());
            }
            let text = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_default();
            Ok(TurnOutput {
                text,
                input_tokens: 10,
                output_tokens: 10,
            })
        }
    }

    #[tokio::test]
    async fn loop_rejects_non_allowlisted_tool_as_error_observation() {
        // First turn asks for a forbidden write_file tool; second turn emits
        // findings. The loop must feed back a rejection and never execute it.
        let runner = ScriptedRunner::new(vec![
            r#"{"tool":"write_file","input":{"path":"x","content":"y"}}"#,
            r#"[{"severity":"info","file":"a.rs","rule":"note","rationale":"ok"}]"#,
        ]);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();
        // Pre-create a file the forbidden write would have targeted.
        std::fs::write(ws.path().join("x"), "original").unwrap();

        let findings = run_audit_loop(
            &runner,
            "seed",
            ws.path(),
            &session,
            &ingot(),
            &vault(),
            &embedder(),
            &LoopBudget::default(),
        )
        .await
        .unwrap();

        assert_eq!(findings.len(), 1, "loop completes with the findings turn");
        // The file must be untouched: the write was never dispatched.
        let content = std::fs::read_to_string(ws.path().join("x")).unwrap();
        assert_eq!(content, "original", "forbidden write must not execute");
        // The rejection must have been fed back as an observation.
        let seen = runner.seen.lock().unwrap();
        assert!(
            seen.iter().any(|s| s.contains("not permitted")),
            "a rejection observation must be fed back; saw: {seen:?}"
        );
    }

    #[tokio::test]
    async fn loop_dispatches_allowlisted_tool() {
        let runner = ScriptedRunner::new(vec![
            r#"{"tool":"list_files","input":{"path":"."}}"#,
            r#"[{"severity":"low","file":"f.rs","rule":"r","rationale":"x"}]"#,
        ]);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join("f.rs"), "fn main() {}").unwrap();

        let findings = run_audit_loop(
            &runner,
            "seed",
            ws.path(),
            &session,
            &ingot(),
            &vault(),
            &embedder(),
            &LoopBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(findings.len(), 1);
        let seen = runner.seen.lock().unwrap();
        assert!(
            seen.iter().any(|s| s.contains("f.rs")),
            "list_files observation must be fed back; saw: {seen:?}"
        );
    }

    #[tokio::test]
    async fn loop_halts_at_max_iterations() {
        // The runner never emits findings: an endless tool-call loop. The
        // iteration bound must stop it.
        let responses: Vec<&str> =
            std::iter::repeat_n(r#"{"tool":"list_files","input":{"path":"."}}"#, 100).collect();
        let runner = ScriptedRunner::new(responses);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();

        let budget = LoopBudget {
            max_iterations: 3,
            token_budget: DEFAULT_TOKEN_BUDGET,
        };
        let findings = run_audit_loop(
            &runner,
            "seed",
            ws.path(),
            &session,
            &ingot(),
            &vault(),
            &embedder(),
            &budget,
        )
        .await
        .unwrap();
        assert!(findings.is_empty(), "no findings when none are emitted");
        // The runner was called at most max_iterations times.
        let calls = runner.seen.lock().unwrap().len();
        assert!(
            calls <= 3,
            "loop must halt at max_iterations; calls={calls}"
        );
    }

    #[tokio::test]
    async fn full_pipeline_loop_persist_render_produces_markdown_report() {
        // End-to-end over the daemon side without a live provider: a scripted
        // review turn surfaces findings, which are persisted and rendered to a
        // markdown report with the per-severity header and severity sections.
        let runner = ScriptedRunner::new(vec![
            r#"{"tool":"list_files","input":{"path":"."}}"#,
            r#"[
                {"severity":"critical","file":"src/db.rs","line":7,"rule":"sql-injection","rationale":"interpolated query"},
                {"severity":"low","file":"src/util.rs","rule":"naming","rationale":"unclear name"}
            ]"#,
        ]);
        let session = review_session();
        let ws = tempfile::tempdir().unwrap();
        let ws_root = ws.path().canonicalize().unwrap();
        std::fs::write(ws_root.join("src.rs"), "fn main() {}").unwrap();
        let ig = ingot();

        let findings = run_audit_loop(
            &runner,
            "audit src/",
            &ws_root,
            &session,
            &ig,
            &vault(),
            &embedder(),
            &LoopBudget::default(),
        )
        .await
        .unwrap();
        assert_eq!(findings.len(), 2);

        persist_findings(&ig, &session.id.to_string(), &findings)
            .await
            .unwrap();
        let events = ig.list_audit_events(&session.id.to_string()).await.unwrap();
        assert!(events.iter().any(|e| e.action_type == "audit_finding"));

        let report = render_report(&findings);
        assert!(report.contains("## Summary"), "header present");
        assert!(report.contains("Critical: 1"));
        assert!(report.contains("## Critical"));
        assert!(report.contains("`src/db.rs:7` — **sql-injection**"));
    }

    #[test]
    fn review_session_denies_write_bash() {
        // The session the loop runs under is in review mode, so the existing
        // gate denies write-arity bash.
        let session = review_session();
        assert!(
            !crate::executor::role_allows_write_bash_for_test(&session),
            "review-mode session must deny write-arity bash"
        );
    }
}
