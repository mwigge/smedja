//! Turn orchestration logic extracted from `run_turn` in `main.rs`.
//!
//! [`TurnOrchestrator`] encapsulates all the dependencies that were previously
//! threaded through the free function `run_turn` as parameters.  Call
//! [`TurnOrchestrator::run`] to execute a single agent turn end-to-end.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use smedja_assayer::Assayer;
use smedja_bellows::Dispatcher;
use smedja_ingot::IngotHandle;
use smedja_vault::Vault;
use tokio::sync::Mutex;

use crate::cowork::CoworkGate;
use crate::price_table::PriceTable;
use crate::provider_pool::ProviderPool;

pub(crate) mod cold;

mod context;

mod routing;
pub(crate) use routing::{gc_provider_sessions, CacheAligners, ProviderSessions};

mod prompt;

mod tools_catalog;

mod turn_run;

// Items surfaced only for the in-module unit tests. The turn pipeline itself
// imports these directly from the `context` / `prompt` / `routing` submodules
// (see `turn_run.rs`); re-exporting them here would warn as unused in a normal
// build, so they are gated to `#[cfg(test)]`.
#[cfg(test)]
use {
    context::{model_context_window, strata_for_tier},
    prompt::{
        build_summariser_prompt, build_turn_context, derive_title, format_lsp_diagnostics,
        format_vault_recalled, methodology_directive_for, sanitize_unicode_tags,
    },
    routing::{
        compact_threshold_from_env, context_pressure_exceeds_threshold, AlignerKey,
        ProviderSessionEntry,
    },
    smedja_telemetry as tel,
};

/// Maps a completed tool-result string to an ACP-shaped tool-call status.
///
/// A result beginning with an error or denial marker is a `Failed` call;
/// anything else is `Completed`. Kept pure so the classification is
/// unit-testable and matches the audit-event status derivation.
fn tool_status_from_result(result: &str) -> smedja_bellows::ToolCallStatus {
    if result.starts_with("error:")
        || result.starts_with("permission denied")
        || result.starts_with("denied")
    {
        smedja_bellows::ToolCallStatus::Failed
    } else {
        smedja_bellows::ToolCallStatus::Completed
    }
}

/// Builds the ACP diff content for an edit tool from its raw input, reading the
/// target file's current contents as `old_text`.
///
/// Returns an empty vec for a non-edit tool (no proposed-content field), so a
/// plain status update carries no content. An edit's proposed content becomes
/// `new_text`, letting an ACP client (Zed) render the change inline for
/// modify-then-approve.
async fn tool_diff_content(
    tool_input: &str,
    workspace_root: &std::path::Path,
) -> Vec<smedja_bellows::ToolCallContent> {
    let Ok(input) = serde_json::from_str::<serde_json::Value>(tool_input) else {
        return Vec::new();
    };
    let Some(new_text) = crate::executor::fs_tools::extract_proposed_content(&input) else {
        return Vec::new();
    };
    let path = input
        .get("path")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let old_text = crate::executor::fs_tools::read_current_content(workspace_root, &path).await;
    vec![smedja_bellows::ToolCallContent::Diff {
        path,
        old_text,
        new_text,
    }]
}

/// Owns all the shared resources needed to execute a single agent turn.
pub(crate) struct TurnOrchestrator {
    ingot: IngotHandle,
    dispatcher: Arc<Dispatcher>,
    gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
    pool: Arc<ProviderPool>,
    assayer: Arc<Assayer>,
    price_table: Arc<PriceTable>,
    vault: Arc<Mutex<Vault>>,
    embedder: Arc<dyn crate::embedder_port::Embedder>,
    provider_sessions: ProviderSessions,
    cache_aligners: CacheAligners,
    active_change: Option<String>,
    lsp_manager: Arc<smedja_lsp::LspManager>,
    max_tool_turns: Option<u32>,
}

impl TurnOrchestrator {
    #[allow(clippy::too_many_arguments)] // forwarded directly from run_turn / loop runner
    pub(crate) fn new(
        ingot: IngotHandle,
        dispatcher: Arc<Dispatcher>,
        gates: Arc<Mutex<HashMap<String, Arc<CoworkGate>>>>,
        pool: Arc<ProviderPool>,
        assayer: Arc<Assayer>,
        price_table: Arc<PriceTable>,
        vault: Arc<Mutex<Vault>>,
        embedder: Arc<dyn crate::embedder_port::Embedder>,
        provider_sessions: ProviderSessions,
        cache_aligners: CacheAligners,
        active_change: Option<String>,
        lsp_manager: Arc<smedja_lsp::LspManager>,
    ) -> Self {
        Self {
            ingot,
            dispatcher,
            gates,
            pool,
            assayer,
            price_table,
            vault,
            embedder,
            provider_sessions,
            cache_aligners,
            active_change,
            lsp_manager,
            max_tool_turns: None,
        }
    }

    /// Overrides the per-turn tool-call cap for loop-runner sessions.
    pub(crate) fn cap_tool_turns(mut self, n: u32) -> Self {
        self.max_tool_turns = Some(n);
        self
    }
}

/// Bounded wait for post-edit diagnostics before giving up (server lag no-op).
const POST_EDIT_DIAG_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// After a successful edit tool (`write_file` / `edit_file` / `move_file`),
/// nudges the language server to re-check the touched files and appends any
/// fresh errors/warnings to the tool result the agent sees.
///
/// Because this lives on the tool-result path it reaches every runner. It is a
/// silent no-op unless: the tool is an edit tool, it succeeded, a language
/// server serves the touched file, and diagnostics arrive within
/// [`POST_EDIT_DIAG_WAIT`]. On timeout it returns `result` unchanged.
async fn append_edit_diagnostics(
    tool_name: &str,
    tool_input: &str,
    result: String,
    lsp_manager: &Arc<smedja_lsp::LspManager>,
) -> String {
    if !matches!(tool_name, "write_file" | "edit_file" | "move_file") {
        return result;
    }
    // Edit tools surface failures as `error:` / `denied:` / `permission denied`.
    let head = result.trim_start();
    if head.starts_with("error")
        || head.starts_with("denied")
        || head.starts_with("permission denied")
    {
        return result;
    }

    let input: serde_json::Value =
        serde_json::from_str(tool_input).unwrap_or(serde_json::Value::Null);
    // Edited paths: `path` (write/edit) and `destination` (move).
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for key in ["path", "destination"] {
        if let Some(p) = input.get(key).and_then(serde_json::Value::as_str) {
            files.push(std::path::PathBuf::from(p));
        }
    }
    if files.is_empty() {
        return result;
    }

    let mut blocks = Vec::new();
    for file in files {
        let diags = lsp_manager
            .refresh_and_wait(&file, POST_EDIT_DIAG_WAIT)
            .await;
        if let Some(block) = format_edit_diagnostics(&file, &diags) {
            blocks.push(block);
        }
    }
    if blocks.is_empty() {
        result
    } else {
        format!("{result}\n{}", blocks.join("\n"))
    }
}

/// Formats the error/warning diagnostics for one edited file into a compact
/// block appended to the tool result. Returns `None` when there is nothing to
/// report (hints/info are dropped as noise for the feedback loop).
fn format_edit_diagnostics(
    file: &std::path::Path,
    diags: &[smedja_lsp::Diagnostic],
) -> Option<String> {
    use smedja_lsp::Severity;
    let relevant: Vec<&smedja_lsp::Diagnostic> = diags
        .iter()
        .filter(|d| matches!(d.severity, Severity::Error | Severity::Warning))
        .collect();
    if relevant.is_empty() {
        return None;
    }
    let mut s = format!("<lsp_diagnostics file=\"{}\">", file.display());
    for d in relevant {
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
            Severity::Hint => "hint",
        };
        let code = d
            .code
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        let _ = write!(
            s,
            "\n{}:{}:{}: {sev}{code}: {}",
            file.display(),
            d.line,
            d.col,
            d.message
        );
    }
    s.push_str("\n</lsp_diagnostics>");
    Some(s)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use smedja_adapter::types::{Delta, Message as AdapterMessage};
    use smedja_adapter::{AdapterError, CallOptions, DeltaStream, Provider};
    use smedja_assayer::{Assayer, Runner, Tier};
    use smedja_bellows::{Dispatcher, TurnEvent};
    use smedja_ingot::{Ingot, IngotHandle, Session, Task};
    use smedja_types::Timestamp;
    use smedja_vault::Vault;
    use tokio::sync::Mutex;
    use uuid::Uuid;

    use crate::price_table::PriceTable;
    use crate::provider_pool::{build_provider_pool, ProviderEntry, ProviderPool};

    use smedja_methodology::MethodologyConfig;

    #[test]
    fn gc_retains_live_session_and_evicts_idle() {
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        let idle = Duration::from_secs(30 * 60);
        let mut map: HashMap<String, super::ProviderSessionEntry> = HashMap::new();
        // A live session touched "now" (an in-flight turn).
        map.insert(
            "live".to_owned(),
            super::ProviderSessionEntry::new("resume-live".to_owned()),
        );
        // Fill past the cap so the GC engages, all stale (touched an hour ago).
        let stale = Instant::now() - Duration::from_secs(60 * 60);
        for i in 0..11_000 {
            map.insert(
                format!("stale-{i}"),
                super::ProviderSessionEntry {
                    id: format!("resume-{i}"),
                    last_used: stale,
                },
            );
        }

        let evicted = super::gc_provider_sessions(&mut map, 10_000, idle);
        assert_eq!(evicted, 11_000, "all idle entries are evicted");
        assert!(
            map.contains_key("live"),
            "an in-flight session must survive GC"
        );
        assert_eq!(map.len(), 1, "only the live session remains");
    }

    #[test]
    fn gc_noop_under_cap() {
        use std::collections::HashMap;
        use std::time::Duration;
        let mut map: HashMap<String, super::ProviderSessionEntry> = HashMap::new();
        map.insert(
            "a".to_owned(),
            super::ProviderSessionEntry::new("x".to_owned()),
        );
        // Under cap: nothing is evicted even though the entry is "old" by idle.
        let evicted = super::gc_provider_sessions(&mut map, 10_000, Duration::from_secs(0));
        assert_eq!(evicted, 0);
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn directive_present_under_default_config() {
        // On a code-writing turn with default config the sealed system prefix
        // carries the TDD/clean discipline directive (both clauses present).
        let directive = super::methodology_directive_for(MethodologyConfig::default(), true)
            .expect("default config must yield a directive");
        assert!(directive.contains("<methodology_discipline>"));
        assert!(directive.contains("failing test"));
        assert!(directive.contains("`unwrap`"));
    }

    #[test]
    fn tdd_clause_omitted_when_tdd_disabled() {
        let cfg = MethodologyConfig {
            tdd: false,
            clean: true,
        };
        let directive = super::methodology_directive_for(cfg, true)
            .expect("clean clause must still be present");
        assert!(!directive.contains("failing test"));
        assert!(directive.contains("`unwrap`"));
    }

    #[test]
    fn clean_clause_omitted_when_clean_disabled() {
        let cfg = MethodologyConfig {
            tdd: true,
            clean: false,
        };
        let directive =
            super::methodology_directive_for(cfg, true).expect("tdd clause must still be present");
        assert!(directive.contains("failing test"));
        assert!(!directive.contains("`unwrap`"));
    }

    #[test]
    fn directive_omitted_entirely_when_both_disabled() {
        let cfg = MethodologyConfig {
            tdd: false,
            clean: false,
        };
        assert!(super::methodology_directive_for(cfg, true).is_none());
    }

    /// A provider that yields a single classified error then nothing — used to
    /// trigger a rotation in the orchestrator.
    struct ErrorProvider {
        kind: &'static str,
    }
    impl Provider for ErrorProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            let err = match self.kind {
                "context_length_exceeded" => {
                    AdapterError::ContextLengthExceeded("prompt is too long".to_owned())
                }
                _ => AdapterError::QuotaExhausted("insufficient_quota".to_owned()),
            };
            Box::pin(futures_util::stream::iter(vec![Err(err)]))
        }
    }

    /// A provider that streams a fixed, tool-call-free text response plus usage.
    struct SuccessProvider {
        text: &'static str,
    }
    impl Provider for SuccessProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            let text = self.text.to_owned();
            Box::pin(futures_util::stream::iter(vec![
                Ok(Delta::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                }),
                Ok(Delta::Text(text)),
            ]))
        }
    }

    /// A provider that reports a fixed cache-read count alongside usage.
    struct CacheReadProvider {
        cache_read_tokens: u32,
    }
    impl Provider for CacheReadProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            Box::pin(futures_util::stream::iter(vec![
                Ok(Delta::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: self.cache_read_tokens,
                }),
                Ok(Delta::Text("done".to_owned())),
            ]))
        }
    }

    fn entry(
        key: (Runner, Tier),
        runner_name: &'static str,
        provider: Box<dyn Provider>,
    ) -> ((Runner, Tier), ProviderEntry) {
        (
            key,
            ProviderEntry {
                provider,
                runner: key.0,
                tier: key.1,
                runner_name,
                default_model: "test-model".to_owned(),
            },
        )
    }

    /// Seeds an in-memory ingot with a session (no mode → Orchestrator route to
    /// Claude/Fast) and a task, returning the handle and the turn id.
    async fn seed_session_and_task(prompt: &str) -> (IngotHandle, String, String) {
        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));
        let session_id = Uuid::new_v4().to_string();
        let task_id = Uuid::new_v4();
        let now = Timestamp::now();
        ingot
            .create_session(Session {
                id: Uuid::parse_str(&session_id).unwrap(),
                created_at: now,
                updated_at: now,
                status: "active".to_owned(),
                task_id: None,
                mode: None,
                title: "test".to_owned(),
                cowork_mode: false,
                workspace_root: None,
                model_override: None,
                runner_override: None,
            })
            .await
            .expect("session insert");
        ingot
            .create_task(Task {
                id: task_id,
                title: prompt.to_owned(),
                description: String::new(),
                status: "planned".to_owned(),
                created_at: now,
                session_id: Some(session_id.clone()),
                response: None,
            })
            .await
            .expect("task insert");
        (ingot, session_id, task_id.to_string())
    }

    fn orchestrator_with_pool(
        ingot: IngotHandle,
        dispatcher: Arc<Dispatcher>,
        pool: ProviderPool,
    ) -> super::TurnOrchestrator {
        super::TurnOrchestrator::new(
            ingot,
            dispatcher,
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(pool),
            Arc::new(Assayer::default_rules()),
            Arc::new(PriceTable::embedded()),
            Arc::new(Mutex::new(
                Vault::open_in_memory().expect("in-memory Vault must open"),
            )),
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            None,
            Arc::new(smedja_lsp::LspManager::new()),
        )
    }

    #[tokio::test]
    async fn rotates_to_next_provider_on_quota_error_preserving_prompt() {
        let (ingot, session_id, turn_id) = seed_session_and_task("solve the problem").await;
        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();

        // Routed entry (Claude/Fast) errors; the more-capable Claude/Deep entry
        // succeeds. The ring is [Fast, Deep].
        let pool = ProviderPool::from_entries_for_test(vec![
            entry(
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
            entry(
                (Runner::Claude, Tier::Deep),
                "claude-deep",
                Box::new(SuccessProvider {
                    text: "answer from second provider",
                }),
            ),
        ]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id, turn_id).await;

        let mut completed = false;
        let mut got_second_provider_text = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                TurnEvent::Completed { .. } => completed = true,
                TurnEvent::AssistantDelta { content, .. } => {
                    if content.contains("answer from second provider") {
                        got_second_provider_text = true;
                    }
                }
                TurnEvent::Failed { reason, .. } => {
                    panic!("turn must not fail on rotation: {reason}")
                }
                _ => {}
            }
        }
        assert!(
            completed,
            "turn must complete after rotating to the second provider"
        );
        assert!(
            got_second_provider_text,
            "the completed turn must carry the second provider's response"
        );
    }

    #[tokio::test]
    async fn turn_fails_after_ring_exhausted_with_last_kind() {
        let (ingot, session_id, turn_id) = seed_session_and_task("solve the problem").await;
        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();

        // Every ring entry yields a retryable quota error.
        let pool = ProviderPool::from_entries_for_test(vec![
            entry(
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
            entry(
                (Runner::Claude, Tier::Deep),
                "claude-deep",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
        ]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id, turn_id).await;

        let mut failure_reason = None;
        while let Ok(ev) = rx.try_recv() {
            if let TurnEvent::Failed { reason, .. } = ev {
                failure_reason = Some(reason);
            }
        }
        let reason = failure_reason.expect("turn must fail when every ring entry errors");
        assert!(
            reason.contains("quota_exhausted"),
            "failure reason must carry the last classified kind, got: {reason}"
        );
    }

    #[tokio::test]
    async fn cache_read_tokens_recorded_as_source_cache() {
        let (ingot, session_id, turn_id) = seed_session_and_task("do the thing").await;
        let dispatcher = Arc::new(Dispatcher::new(64));

        // The routed entry (Claude/Fast) reports 1234 cache-read tokens.
        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            Box::new(CacheReadProvider {
                cache_read_tokens: 1234,
            }),
        )]);

        let orc = orchestrator_with_pool(ingot.clone(), Arc::clone(&dispatcher), pool);
        orc.run(session_id.clone(), turn_id).await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        let cache_total: i64 = by_source
            .iter()
            .filter(|(src, _)| src == "cache")
            .map(|(_, n)| *n)
            .sum();
        assert_eq!(
            cache_total, 1234,
            "a turn reporting cache_read_input_tokens=N must write source=cache, tokens_saved=N"
        );
    }

    #[tokio::test]
    async fn zero_cache_reads_write_no_cache_row() {
        let (ingot, session_id, turn_id) = seed_session_and_task("do the thing").await;
        let dispatcher = Arc::new(Dispatcher::new(64));

        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            Box::new(CacheReadProvider {
                cache_read_tokens: 0,
            }),
        )]);

        let orc = orchestrator_with_pool(ingot.clone(), Arc::clone(&dispatcher), pool);
        orc.run(session_id.clone(), turn_id).await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        assert!(
            !by_source.iter().any(|(src, _)| src == "cache"),
            "a zero cache-read turn must write no source=cache row"
        );
    }

    #[tokio::test]
    async fn rotation_records_error_kind_and_retryable() {
        use opentelemetry_sdk::testing::trace::InMemorySpanExporter;
        use opentelemetry_sdk::trace::TracerProvider;

        let exporter = InMemorySpanExporter::default();
        let provider = TracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        opentelemetry::global::set_tracer_provider(provider.clone());

        let (ingot, session_id, turn_id) = seed_session_and_task("solve the problem").await;
        let dispatcher = Arc::new(Dispatcher::new(64));

        let pool = ProviderPool::from_entries_for_test(vec![
            entry(
                (Runner::Claude, Tier::Fast),
                "claude-cli",
                Box::new(ErrorProvider {
                    kind: "quota_exhausted",
                }),
            ),
            entry(
                (Runner::Claude, Tier::Deep),
                "claude-deep",
                Box::new(SuccessProvider {
                    text: "answer from second provider",
                }),
            ),
        ]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id.clone(), turn_id.clone()).await;

        let _ = provider.force_flush();
        let spans = exporter.get_finished_spans().expect("spans");
        // Locate this turn's agent-invoke span by its unique turn id attribute.
        let turn_span = spans
            .iter()
            .filter(|s| s.name == super::tel::SPAN_AGENT_INVOKE)
            .find(|s| {
                s.attributes.iter().any(|kv| {
                    kv.key.as_str() == "smedja.turn.id" && kv.value.as_str() == turn_id.as_str()
                })
            })
            .expect("this turn's agent-invoke span must be exported");

        let kind = turn_span
            .attributes
            .iter()
            .find(|kv| kv.key.as_str() == "smedja.error.kind")
            .map(|kv| kv.value.as_str().to_string());
        assert_eq!(
            kind.as_deref(),
            Some("quota_exhausted"),
            "rotation must record smedja.error.kind on the turn span"
        );
        assert!(
            turn_span
                .attributes
                .iter()
                .any(|kv| kv.key.as_str() == "smedja.error.retryable"),
            "rotation must record smedja.error.retryable on the turn span"
        );
    }

    #[test]
    fn fast_tier_prompt_no_larger_than_deep_with_hot_present() {
        use smedja_adapter::types::Message;
        use smedja_assayer::Tier;
        use smedja_memory::WorkingMemory;

        let build = |tier: Tier| {
            let (strata, budget) = super::strata_for_tier(tier);
            let mut m = WorkingMemory::new(budget);
            m.set_strata(strata);
            m.push(Message::user("stable context")); // prefix
            m.seal_prefix();
            for i in 0..40 {
                m.push(Message::user(format!(
                    "turn {i} with enough content to cost a few tokens each"
                )));
            }
            m.build_prompt(budget)
        };

        let fast = build(Tier::Fast);
        let deep = build(Tier::Deep);

        // A shallower/cheaper tier must never assemble more messages than deep.
        assert!(
            fast.len() <= deep.len(),
            "fast prompt ({}) must be ≤ deep prompt ({})",
            fast.len(),
            deep.len()
        );
        // The most recent hot turn must be present in both regardless of tier.
        assert!(
            fast.iter().any(|m| m.content.contains("turn 39")),
            "fast must retain the latest hot turn"
        );
        assert!(
            deep.iter().any(|m| m.content.contains("turn 39")),
            "deep must retain the latest hot turn"
        );
    }

    #[test]
    fn model_context_window_known_and_default() {
        assert_eq!(super::model_context_window("claude-sonnet-4-6"), 200_000);
        assert_eq!(super::model_context_window("some-unknown-model"), 128_000);
    }

    #[tokio::test]
    async fn orchestrator_returns_error_for_unknown_session() {
        use smedja_bellows::TurnEvent;

        // Arrange: build the shared dispatcher first so we can subscribe before run().
        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));
        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();
        let gates = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let pool = Arc::new(build_provider_pool().await);
        let assayer = Arc::new(Assayer::default_rules());
        let price_table = Arc::new(PriceTable::embedded());
        let vault = Arc::new(Mutex::new(
            Vault::open_in_memory().expect("in-memory Vault must open"),
        ));

        let provider_sessions = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let cache_aligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let orc = super::TurnOrchestrator::new(
            ingot,
            Arc::clone(&dispatcher),
            gates,
            pool,
            assayer,
            price_table,
            vault,
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            provider_sessions,
            cache_aligners,
            None,
            Arc::new(smedja_lsp::LspManager::new()),
        );

        let session_id = "sess-does-not-exist".to_owned();
        let turn_id = "turn-does-not-exist".to_owned();

        // Act: run the orchestrator with an unknown turn_id.
        orc.run(session_id.clone(), turn_id.clone()).await;

        // Assert: a Fail event must have been published.
        let mut got_fail = false;
        while let Ok(ev) = rx.try_recv() {
            if matches!(ev, TurnEvent::Failed { .. }) {
                got_fail = true;
                break;
            }
        }
        assert!(
            got_fail,
            "orchestrator must publish TurnEvent::Failed for an unknown task"
        );
    }

    /// Cross-turn persistence of the per-`(session, runner)` aligner.
    ///
    /// These tests model exactly what the ring loop does: look up (or default)
    /// the aligner for a key in the shared [`super::CacheAligners`] map, call
    /// `align` against the freshly-sealed memory, and store the mutated aligner
    /// back. A persisted aligner observes the prior turn and reports real
    /// `Grown`/`Mutated` drift; distinct runner keys never share history.
    mod cache_aligner_persistence {
        use std::sync::Arc;

        use smedja_adapter::types::Message as AdapterMessage;
        use smedja_memory::{Drift, WorkingMemory};
        use tokio::sync::Mutex;

        use crate::orchestrator::{AlignerKey, CacheAligners};

        /// Builds a sealed [`WorkingMemory`] whose stable prefix is `prefix`.
        fn sealed(prefix: &[&str]) -> WorkingMemory {
            let mut mem = WorkingMemory::new(4096);
            for content in prefix {
                mem.push(AdapterMessage::system(*content));
            }
            mem.seal_prefix();
            mem
        }

        /// Mirrors the ring-loop get-or-insert: take-or-default under the lock,
        /// align, re-insert, and return the hint.
        async fn align_persisted(
            aligners: &CacheAligners,
            key: &AlignerKey,
            mem: &WorkingMemory,
        ) -> Drift {
            let mut guard = aligners.lock().await;
            let mut aligner = guard.remove(key).unwrap_or_default();
            let hint = aligner.align(mem);
            guard.insert(key.clone(), aligner);
            hint.drift
        }

        #[tokio::test]
        async fn second_turn_same_session_runner_reports_grown() {
            let aligners: CacheAligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let key: AlignerKey = ("sess-1".to_owned(), "anthropic".to_owned());

            let first = align_persisted(&aligners, &key, &sealed(&["sys", "skills"])).await;
            assert_eq!(first, Drift::Unchanged, "first turn has no prior history");

            // Same leading messages, prefix grew by one settled turn.
            let second =
                align_persisted(&aligners, &key, &sealed(&["sys", "skills", "settled turn"])).await;
            assert_eq!(
                second,
                Drift::Grown,
                "a persisted aligner must observe the prior boundary and report Grown, not a fresh Unchanged"
            );
        }

        #[tokio::test]
        async fn distinct_runner_keys_do_not_share_history() {
            let aligners: CacheAligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let anthropic: AlignerKey = ("sess-1".to_owned(), "anthropic".to_owned());
            let openai: AlignerKey = ("sess-1".to_owned(), "openai".to_owned());

            // Anthropic observes a grown prefix across two turns.
            let _ = align_persisted(&aligners, &anthropic, &sealed(&["sys", "skills"])).await;
            let grown = align_persisted(
                &aligners,
                &anthropic,
                &sealed(&["sys", "skills", "settled"]),
            )
            .await;
            assert_eq!(grown, Drift::Grown);

            // A failover to openai (same session) must start fresh: first turn is
            // Unchanged at the full prefix, never compared against anthropic's history.
            let openai_first =
                align_persisted(&aligners, &openai, &sealed(&["sys", "skills", "settled"])).await;
            assert_eq!(
                openai_first,
                Drift::Unchanged,
                "a fresh runner key must not inherit the prior runner's prefix digests"
            );
        }

        #[tokio::test]
        async fn mutated_message_inside_prior_boundary_reports_mutated() {
            let aligners: CacheAligners = Arc::new(Mutex::new(std::collections::HashMap::new()));
            let key: AlignerKey = ("sess-1".to_owned(), "anthropic".to_owned());

            let _ = align_persisted(&aligners, &key, &sealed(&["sys", "skills", "context"])).await;

            // Second turn: index 1 changed content inside the prior boundary.
            let second =
                align_persisted(&aligners, &key, &sealed(&["sys", "CHANGED", "context"])).await;
            assert_eq!(
                second,
                Drift::Mutated,
                "a message changing inside the prior sealed boundary must report Mutated"
            );
        }
    }

    // --- derive_title tests ---

    #[test]
    fn derive_title_takes_first_ten_words() {
        let input = "one two three four five six seven eight nine ten eleven twelve";
        let title = super::derive_title(input);
        assert_eq!(title, "one two three four five six seven eight nine ten");
    }

    #[test]
    fn derive_title_short_input_unchanged() {
        let title = super::derive_title("fix the bug");
        assert_eq!(title, "fix the bug");
    }

    #[test]
    fn derive_title_strips_graph_injection_block() {
        let input = "refactor auth module\n\n<graph_symbols>\nsome code\n</graph_symbols>";
        let title = super::derive_title(input);
        assert_eq!(title, "refactor auth module");
    }

    #[test]
    fn derive_title_empty_input_returns_empty() {
        assert_eq!(super::derive_title(""), "");
    }

    // --- format_lsp_diagnostics tests ---

    #[test]
    fn format_lsp_diagnostics_empty_snapshot_returns_none() {
        let snap = smedja_lsp::LspSnapshot::default();
        assert!(super::format_lsp_diagnostics(&snap).is_none());
    }

    #[test]
    fn format_lsp_diagnostics_errors_and_warnings_included() {
        use smedja_lsp::types::{Diagnostic, Severity};
        use std::path::PathBuf;
        let snap = smedja_lsp::LspSnapshot {
            servers: vec![],
            diagnostics: vec![
                Diagnostic {
                    file: PathBuf::from("src/main.rs"),
                    line: 42,
                    col: 1,
                    severity: Severity::Error,
                    code: Some("E0308".to_owned()),
                    message: "mismatched types".to_owned(),
                },
                Diagnostic {
                    file: PathBuf::from("src/lib.rs"),
                    line: 17,
                    col: 5,
                    severity: Severity::Warning,
                    code: None,
                    message: "unused variable".to_owned(),
                },
            ],
        };
        let block = super::format_lsp_diagnostics(&snap).unwrap();
        assert!(block.contains("<lsp_diagnostics>"));
        assert!(block.contains("src/main.rs:42"));
        assert!(block.contains("mismatched types"));
        assert!(block.contains("src/lib.rs:17"));
        assert!(block.contains("unused variable"));
    }

    #[test]
    fn format_lsp_diagnostics_caps_at_twenty_lines() {
        use smedja_lsp::types::{Diagnostic, Severity};
        use std::path::PathBuf;
        let diags: Vec<Diagnostic> = (0..30)
            .map(|i| Diagnostic {
                file: PathBuf::from("src/main.rs"),
                line: i,
                col: 1,
                severity: Severity::Error,
                code: None,
                message: format!("err {i}"),
            })
            .collect();
        let snap = smedja_lsp::LspSnapshot {
            servers: vec![],
            diagnostics: diags,
        };
        let block = super::format_lsp_diagnostics(&snap).unwrap();
        let lines: Vec<&str> = block.lines().collect();
        // header + up to 20 diag lines + footer + optional truncation line
        assert!(lines.len() <= 23, "too many lines: {}", lines.len());
    }

    // --- build_summariser_prompt tests ---

    #[test]
    fn build_summariser_prompt_includes_history() {
        let history = vec![
            ("user".to_owned(), "fix the auth bug".to_owned()),
            (
                "assistant".to_owned(),
                "I found the issue in auth.rs".to_owned(),
            ),
        ];
        let prompt = super::build_summariser_prompt(&history);
        assert!(prompt.contains("fix the auth bug"));
        assert!(prompt.contains("I found the issue in auth.rs"));
    }

    #[test]
    fn build_summariser_prompt_has_instruction() {
        let prompt = super::build_summariser_prompt(&[]);
        assert!(prompt.contains("summarise") || prompt.contains("summary"));
    }

    #[test]
    fn build_summariser_prompt_caps_turns() {
        let history: Vec<(String, String)> = (0..30)
            .map(|i| ("user".to_owned(), format!("turn {i}")))
            .collect();
        let prompt = super::build_summariser_prompt(&history);
        // Should not include all 30 turns verbatim — cap enforced
        let turn_count = prompt.matches("turn ").count();
        assert!(turn_count <= 20, "too many turns: {turn_count}");
    }

    // --- context_pressure_exceeds_threshold tests ---

    #[test]
    fn pressure_below_threshold_is_not_exceeded() {
        assert!(!super::context_pressure_exceeds_threshold(
            79_999, 100_000, 0.85
        ));
    }

    #[test]
    fn pressure_at_threshold_is_exceeded() {
        assert!(super::context_pressure_exceeds_threshold(
            85_000, 100_000, 0.85
        ));
    }

    #[test]
    fn pressure_with_zero_window_is_never_exceeded() {
        assert!(!super::context_pressure_exceeds_threshold(1_000, 0, 0.85));
    }

    #[test]
    fn pressure_with_custom_threshold_respects_it() {
        assert!(super::context_pressure_exceeds_threshold(
            75_000, 100_000, 0.70
        ));
        assert!(!super::context_pressure_exceeds_threshold(
            74_999, 100_000, 0.75
        ));
    }

    #[test]
    fn compact_threshold_clamps_below_half() {
        // Values below 0.5 are clamped to 0.5 — safety guard.
        assert!(super::compact_threshold_from_env(Some("0.3")) >= 0.5);
    }

    #[test]
    fn compact_threshold_default_is_eighty_five_percent() {
        assert!((super::compact_threshold_from_env(None) - 0.85).abs() < f64::EPSILON);
    }

    #[test]
    fn compact_threshold_reads_env_value() {
        assert!((super::compact_threshold_from_env(Some("0.90")) - 0.90).abs() < f64::EPSILON);
    }

    // --- format_vault_recalled tests ---

    fn make_vault_entry(content: &str) -> smedja_vault::VaultEntry {
        smedja_vault::VaultEntry {
            id: "test-id".into(),
            embedding: vec![0.1; 128],
            payload: serde_json::Value::Null,
            namespace: "default".into(),
            content: content.into(),
            source_file: None,
            added_by: None,
            chunk_index: None,
            parent_id: None,
            created_at: 0.0,
            embedder_model_id: "fnv-bow-128".into(),
            dim: 128,
        }
    }

    #[test]
    fn format_vault_recalled_empty_returns_none() {
        assert!(super::format_vault_recalled(&[]).is_none());
    }

    #[test]
    fn format_vault_recalled_single_entry_wraps_in_xml() {
        let entries = vec![make_vault_entry("the auth token expires after 24 hours")];
        let result = super::format_vault_recalled(&entries).unwrap();
        assert!(result.starts_with("<recalled_context>"));
        assert!(result.contains("auth token expires after 24 hours"));
        assert!(result.ends_with("</recalled_context>"));
    }

    #[test]
    fn format_vault_recalled_multiple_entries_joined_with_separator() {
        let entries = vec![make_vault_entry("note one"), make_vault_entry("note two")];
        let result = super::format_vault_recalled(&entries).unwrap();
        assert!(result.contains("note one"));
        assert!(result.contains("note two"));
        assert!(result.contains("---"));
    }

    // --- parallel tool batch tests ---

    /// A stateful provider: first call returns N embedded JSON tool calls; second
    /// call returns `final_text` after receiving the combined tool_result message.
    struct MultiToolProvider {
        calls: Vec<String>,
        final_text: &'static str,
        call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for MultiToolProvider {
        fn stream_chat(&self, messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            use smedja_adapter::types::Role;
            let n = self
                .call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                let text = self.calls.join("\n");
                Box::pin(futures_util::stream::iter(vec![
                    Ok(Delta::Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        cache_read_tokens: 0,
                    }),
                    Ok(Delta::Text(text)),
                ]))
            } else {
                // Verify the combined result was injected before our final text.
                let last_user = messages
                    .iter()
                    .rev()
                    .find(|m| m.role == Role::User)
                    .map(|m| m.content.clone())
                    .unwrap_or_default();
                assert!(
                    last_user.contains("<tool_result"),
                    "second model call must receive combined tool_result; got: {last_user}"
                );
                let text = self.final_text.to_owned();
                Box::pin(futures_util::stream::iter(vec![
                    Ok(Delta::Usage {
                        input_tokens: 5,
                        output_tokens: 3,
                        cache_read_tokens: 0,
                    }),
                    Ok(Delta::Text(text)),
                ]))
            }
        }
    }

    async fn make_ws_session_and_task(
        ingot: &IngotHandle,
        ws_path: std::path::PathBuf,
        title: &str,
    ) -> (String, String) {
        let sid = Uuid::new_v4().to_string();
        let task_id = Uuid::new_v4();
        let now = Timestamp::now();
        ingot
            .create_session(Session {
                id: Uuid::parse_str(&sid).unwrap(),
                created_at: now,
                updated_at: now,
                status: "active".to_owned(),
                task_id: None,
                mode: None,
                title: title.to_owned(),
                cowork_mode: false,
                workspace_root: Some(ws_path.to_string_lossy().to_string()),
                model_override: None,
                runner_override: None,
            })
            .await
            .unwrap();
        ingot
            .create_task(Task {
                id: task_id,
                title: title.to_owned(),
                description: String::new(),
                status: "planned".to_owned(),
                created_at: now,
                session_id: Some(sid.clone()),
                response: None,
            })
            .await
            .unwrap();
        (sid, task_id.to_string())
    }

    fn collect_delta_text(rx: &mut tokio::sync::broadcast::Receiver<TurnEvent>) -> String {
        let mut out = String::new();
        while let Ok(ev) = rx.try_recv() {
            if let TurnEvent::AssistantDelta { content, .. } = ev {
                out.push_str(&content);
            }
        }
        out
    }

    #[tokio::test]
    async fn parallel_read_batch_injects_combined_result() {
        let ws = tempfile::tempdir().unwrap();
        let ws_path = ws.path().to_owned();
        std::fs::write(ws_path.join("a.txt"), b"alpha").unwrap();
        std::fs::write(ws_path.join("b.txt"), b"beta").unwrap();
        std::fs::write(ws_path.join("c.txt"), b"gamma").unwrap();

        let calls = vec![
            r#"{"tool":"read_file","input":{"path":"a.txt"}}"#.to_owned(),
            r#"{"tool":"read_file","input":{"path":"b.txt"}}"#.to_owned(),
            r#"{"tool":"read_file","input":{"path":"c.txt"}}"#.to_owned(),
        ];
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = MultiToolProvider {
            calls,
            final_text: "files read",
            call_count: std::sync::Arc::clone(&call_count),
        };

        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));
        let (session_id, turn_id) =
            make_ws_session_and_task(&ingot, ws_path.clone(), "read files").await;

        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();
        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            Box::new(provider),
        )]);

        let orc = orchestrator_with_pool(ingot, Arc::clone(&dispatcher), pool);
        orc.run(session_id, turn_id).await;

        let delta_text = collect_delta_text(&mut rx);
        assert!(
            delta_text.contains("files read"),
            "final delta must contain the provider's final text; got: {delta_text}"
        );
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly two model calls: one for tool calls, one for final response"
        );
    }

    #[tokio::test]
    async fn mixed_batch_runs_reads_then_write_sequentially() {
        let ws = tempfile::tempdir().unwrap();
        let ws_path = ws.path().to_owned();
        std::fs::write(ws_path.join("src.txt"), b"source content").unwrap();

        let calls = vec![
            r#"{"tool":"read_file","input":{"path":"src.txt"}}"#.to_owned(),
            r#"{"tool":"write_file","input":{"path":"out.txt","content":"hello"}}"#.to_owned(),
        ];
        let call_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider = MultiToolProvider {
            calls,
            final_text: "mixed done",
            call_count: std::sync::Arc::clone(&call_count),
        };

        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));

        // Use "impl" mode (AgentRole::Impl, not read-only). Impl+Coding routes
        // Local/Local; Claude/Deep is compatible (rank 2 >= rank 1) so the
        // Deep pool entry is used as fallback.
        let sid = Uuid::new_v4().to_string();
        let task_id = Uuid::new_v4();
        let now = Timestamp::now();
        ingot
            .create_session(Session {
                id: Uuid::parse_str(&sid).unwrap(),
                created_at: now,
                updated_at: now,
                status: "active".to_owned(),
                task_id: None,
                mode: Some("impl".to_owned()),
                title: "mixed batch".to_owned(),
                cowork_mode: false,
                workspace_root: Some(ws_path.to_string_lossy().to_string()),
                model_override: None,
                runner_override: None,
            })
            .await
            .unwrap();
        ingot
            .create_task(Task {
                id: task_id,
                title: "mixed batch".to_owned(),
                description: String::new(),
                status: "planned".to_owned(),
                created_at: now,
                session_id: Some(sid.clone()),
                response: None,
            })
            .await
            .unwrap();
        let (session_id, turn_id) = (sid, task_id.to_string());

        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();

        // Pre-set the cowork gate to Auto so write_file is auto-approved.
        let gates: Arc<Mutex<std::collections::HashMap<String, Arc<crate::cowork::CoworkGate>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        {
            let gate = Arc::new(crate::cowork::CoworkGate::default());
            gate.set_mode(crate::cowork::PermissionMode::Auto).await;
            gates.lock().await.insert(session_id.clone(), gate);
        }

        // Claude/Deep is compatible with Impl+Coding routing (Local/Local fallback).
        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Deep),
            "claude-deep",
            Box::new(provider),
        )]);

        let orc = super::TurnOrchestrator::new(
            ingot,
            Arc::clone(&dispatcher),
            gates,
            Arc::new(pool),
            Arc::new(Assayer::default_rules()),
            Arc::new(PriceTable::embedded()),
            Arc::new(Mutex::new(Vault::open_in_memory().expect("vault"))),
            Arc::new(crate::embedder_port::FnvEmbedder::new()),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            Arc::new(Mutex::new(std::collections::HashMap::new())),
            None,
            Arc::new(smedja_lsp::LspManager::new()),
        );
        orc.run(session_id, turn_id).await;

        let delta_text = collect_delta_text(&mut rx);
        // The turn must complete: both read (natively handled) and write (MCP-dispatched)
        // results were combined and the model received them before emitting final text.
        assert!(
            delta_text.contains("mixed done"),
            "mixed batch delta must contain final text; got: {delta_text}"
        );
        assert_eq!(
            call_count.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly two model calls: one for batch tool calls, one for final response"
        );
    }

    /// A provider that always emits a single tool call and never a tool-free
    /// reply — it can never terminate the tool loop, so the loop exhausts its cap.
    struct AlwaysToolProvider {
        call_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Provider for AlwaysToolProvider {
        fn stream_chat(&self, _messages: &[AdapterMessage], _opts: &CallOptions) -> DeltaStream {
            self.call_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = r#"{"tool":"read_file","input":{"path":"a.txt"}}"#.to_owned();
            Box::pin(futures_util::stream::iter(vec![
                Ok(Delta::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                }),
                Ok(Delta::Text(text)),
            ]))
        }
    }

    #[tokio::test]
    async fn tool_cap_exhaustion_fails_turn_not_persisted_as_answer() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let ws = tempfile::tempdir().unwrap();
        let ws_path = ws.path().to_owned();
        std::fs::write(ws_path.join("a.txt"), b"alpha").unwrap();

        let ingot = IngotHandle::new(Ingot::open_in_memory().expect("in-memory Ingot must open"));
        let (session_id, turn_id) =
            make_ws_session_and_task(&ingot, ws_path.clone(), "loop without answering").await;

        let call_count = std::sync::Arc::new(AtomicUsize::new(0));
        let provider = AlwaysToolProvider {
            call_count: std::sync::Arc::clone(&call_count),
        };

        let dispatcher = Arc::new(Dispatcher::new(64));
        let mut rx = dispatcher.subscribe();
        let pool = ProviderPool::from_entries_for_test(vec![entry(
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            Box::new(provider),
        )]);

        let cap: u32 = 3;
        let orc = orchestrator_with_pool(ingot.clone(), Arc::clone(&dispatcher), pool)
            .cap_tool_turns(cap);
        orc.run(session_id.clone(), turn_id.clone()).await;

        // The model never produced a tool-free answer, so the turn must fail rather
        // than persist the last tool-call JSON as if it were the final response.
        let task = ingot
            .get_task(&turn_id)
            .await
            .unwrap()
            .expect("task must exist");
        assert_eq!(
            task.status, "failed",
            "a cap-exhausted turn must be marked failed"
        );
        assert!(
            task.response.as_deref().unwrap_or("").is_empty(),
            "raw tool-call JSON must not be persisted as the answer; got: {:?}",
            task.response
        );
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            cap as usize,
            "the model is called exactly `cap` times before exhaustion"
        );

        let mut saw_cap_fail = false;
        while let Ok(ev) = rx.try_recv() {
            if let TurnEvent::Failed { reason, .. } = ev {
                if reason.contains("tool-turn cap") {
                    saw_cap_fail = true;
                }
            }
        }
        assert!(
            saw_cap_fail,
            "a fail event must surface the tool-turn cap exhaustion"
        );
    }

    // --- WI-025: sanitize_unicode_tags ---

    #[test]
    fn sanitize_unicode_tags_strips_private_use_area_block() {
        // U+E0000 tag block (used for prompt injection via Unicode tags)
        let injected = "hello\u{E0001}world\u{E007F}!";
        let clean = super::sanitize_unicode_tags(injected);
        assert_eq!(clean, "helloworld!");
    }

    #[test]
    fn sanitize_unicode_tags_leaves_normal_text_intact() {
        let normal = "Hello, World! こんにちは 🦀";
        assert_eq!(super::sanitize_unicode_tags(normal), normal);
    }

    // --- WI-026: build_turn_context ---

    #[test]
    fn build_turn_context_contains_date_and_cwd() {
        let ctx = super::build_turn_context("2026-06-30", "/home/morgan/project");
        assert!(ctx.starts_with("<turn-context>"), "must open tag");
        assert!(ctx.ends_with("</turn-context>"), "must close tag");
        assert!(ctx.contains("2026-06-30"), "must include date");
        assert!(ctx.contains("/home/morgan/project"), "must include cwd");
    }

    #[test]
    fn build_turn_context_is_stable_across_calls_same_input() {
        let a = super::build_turn_context("2026-06-30", "/repo");
        let b = super::build_turn_context("2026-06-30", "/repo");
        assert_eq!(
            a, b,
            "same inputs must produce identical output for cache stability"
        );
    }

    // ── post-edit diagnostics feedback loop ────────────────────────────────

    fn diag(sev: smedja_lsp::Severity, msg: &str) -> smedja_lsp::Diagnostic {
        smedja_lsp::Diagnostic {
            file: std::path::PathBuf::from("src/lib.rs"),
            line: 7,
            col: 3,
            severity: sev,
            code: Some("E0001".to_owned()),
            message: msg.to_owned(),
        }
    }

    #[test]
    fn format_edit_diagnostics_reports_errors_and_warnings() {
        use smedja_lsp::Severity;
        let file = std::path::Path::new("src/lib.rs");
        let block =
            super::format_edit_diagnostics(file, &[diag(Severity::Error, "mismatched types")])
                .expect("an error must produce a block");
        assert!(block.contains("<lsp_diagnostics file=\"src/lib.rs\">"));
        assert!(block.contains("src/lib.rs:7:3: error [E0001]: mismatched types"));
        assert!(block.trim_end().ends_with("</lsp_diagnostics>"));
    }

    #[test]
    fn format_edit_diagnostics_ignores_hints_and_empty() {
        use smedja_lsp::Severity;
        let file = std::path::Path::new("src/lib.rs");
        assert!(super::format_edit_diagnostics(file, &[]).is_none());
        assert!(
            super::format_edit_diagnostics(file, &[diag(Severity::Hint, "unused")]).is_none(),
            "hint-only diagnostics are dropped as feedback noise"
        );
    }

    #[tokio::test]
    async fn append_edit_diagnostics_is_noop_for_non_edit_and_errors() {
        let mgr = Arc::new(smedja_lsp::LspManager::new());
        // Non-edit tool: returned unchanged.
        let out = super::append_edit_diagnostics(
            "read_file",
            r#"{"path":"a.rs"}"#,
            "file body".to_owned(),
            &mgr,
        )
        .await;
        assert_eq!(out, "file body");

        // Edit tool but a failed result: returned unchanged (never queries LSP).
        let out = super::append_edit_diagnostics(
            "write_file",
            r#"{"path":"a.rs","content":"x"}"#,
            "error: disk full".to_owned(),
            &mgr,
        )
        .await;
        assert_eq!(out, "error: disk full");

        // Edit success but no language server serves the file: no append, and
        // the bounded wait returns promptly rather than hanging.
        let out = super::append_edit_diagnostics(
            "write_file",
            r#"{"path":"a.txt","content":"x"}"#,
            "wrote 1 byte".to_owned(),
            &mgr,
        )
        .await;
        assert_eq!(out, "wrote 1 byte");
    }

    // --- ACP tool-call lifecycle helpers (Item A part 2) ---

    #[test]
    fn tool_status_from_result_classifies_success_and_failure() {
        use smedja_bellows::ToolCallStatus;
        assert_eq!(
            super::tool_status_from_result("wrote 3 bytes"),
            ToolCallStatus::Completed
        );
        assert_eq!(
            super::tool_status_from_result("error: disk full"),
            ToolCallStatus::Failed
        );
        assert_eq!(
            super::tool_status_from_result("permission denied: bash"),
            ToolCallStatus::Failed
        );
        assert_eq!(
            super::tool_status_from_result("denied: read-only role"),
            ToolCallStatus::Failed
        );
    }

    #[tokio::test]
    async fn tool_diff_content_builds_diff_for_edit() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "old body").unwrap();
        let content =
            super::tool_diff_content(r#"{"path":"a.rs","content":"new body"}"#, tmp.path()).await;
        assert_eq!(content.len(), 1);
        match &content[0] {
            smedja_bellows::ToolCallContent::Diff {
                path,
                old_text,
                new_text,
            } => {
                assert_eq!(path, "a.rs");
                assert_eq!(old_text, "old body");
                assert_eq!(new_text, "new body");
            }
        }
    }

    #[tokio::test]
    async fn tool_diff_content_empty_for_non_edit_tool() {
        let tmp = tempfile::tempdir().unwrap();
        // A bash call has no proposed-content field → no diff.
        let content = super::tool_diff_content(r#"{"command":"ls"}"#, tmp.path()).await;
        assert!(content.is_empty());
    }
}
