//! Command-output filtering, large-response offload, and tokens-saved accounting
//! on the tool-result return path.
//!
//! [`filter_command_output`] is the single entry point used by the bash/run_command
//! handler; the in-memory recovery store it populates is read back by the
//! `smedja_retrieve` tool.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use smedja_ingot::{IngotHandle, Session};
use smedja_vault::{Vault, VaultEntry};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::executor::{FILTER_RECOVERY_NAMESPACE, LARGE_RESPONSE_THRESHOLD};

/// Shared mutex serialising process-global env-var mutations across the executor
/// test suite. All env/cwd-mutating tests in this module tree lock this single
/// instance so cargo's multithreaded runner cannot race them.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// In-memory store for content blocks addressed by SHA-256 hash.
/// Used by the `smedja_retrieve` tool to look up compressed context blocks.
pub(crate) fn retrieve_store() -> &'static tokio::sync::Mutex<HashMap<String, String>> {
    static STORE: OnceLock<tokio::sync::Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

/// Insertion-order tracker for the retrieve store LRU eviction.
fn retrieve_store_order() -> &'static tokio::sync::Mutex<std::collections::VecDeque<String>> {
    static ORDER: OnceLock<tokio::sync::Mutex<std::collections::VecDeque<String>>> =
        OnceLock::new();
    ORDER.get_or_init(|| tokio::sync::Mutex::new(std::collections::VecDeque::new()))
}

/// Writes a large tool response to a temp file and returns a compact agent
/// reference. Returns `None` when the write fails; caller falls through to
/// verbatim return.
fn offload_large_response(result: &str, cmd: &str) -> Option<String> {
    let dir = std::env::temp_dir().join("smedja-tool-responses");
    std::fs::create_dir_all(&dir).ok()?;
    let hash = content_hash(result);
    let path = dir.join(&hash);
    std::fs::write(&path, result.as_bytes()).ok()?;
    tracing::debug!(cmd, bytes = result.len(), path = %path.display(), "large response offloaded");
    Some(format!(
        "[large output: {} bytes — use read_file(\"{}\") to retrieve]\n[smedja_retrieve hash={hash} to expand]",
        result.len(),
        path.display(),
    ))
}

/// Computes the lowercase hex SHA-256 content hash used to address a teed
/// full-output recovery entry.
pub(crate) fn content_hash(content: &str) -> String {
    use sha2::{Digest as _, Sha256};
    format!("{:x}", Sha256::digest(content.as_bytes()))
}

/// Applies command-aware text filtering to a `bash`/`run_command` result on the
/// return path, in-process (no shell hooks, no subprocess).
///
/// JSON results route to [`smedja_adapter::compress_tool_result`] (`SmartCrusher`,
/// unchanged); text routes through the command-keyed filter registry loaded from
/// `.smedja/filters.toml`. When filtering reduces the output (ratio < 1.0) the
/// full uncompressed text is teed to the vault recovery namespace and registered
/// in the `smedja_retrieve` store under its content hash, a trailing recovery
/// marker naming that hash is appended, and the estimated tokens saved are
/// recorded on the tokens-saved ledger. The captured success/failure contract
/// of the result is never altered — only its body text is compressed.
///
/// `SMEDJA_NO_TOOL_COMPRESS=1` is honoured by the underlying compressors and
/// returns the result verbatim.
pub(crate) async fn filter_command_output(
    cmd: &str,
    result: String,
    workspace: &std::path::Path,
    session: Option<&Session>,
    ingot: &IngotHandle,
    vault: &Arc<Mutex<Vault>>,
) -> String {
    // Single branch point: JSON routes through SmartCrusher; text routes through
    // the command filter. The bash/run_command path is text by construction.
    if serde_json::from_str::<Value>(&result).is_ok() {
        let compressed = smedja_adapter::compress_tool_result(&result);
        // Attribute the SmartCrusher saving to its own source so it is not folded
        // into the filter total. The estimate is recorded on this JSON path only.
        record_tokens_saved(cmd, &result, &compressed, "crusher", session, ingot).await;
        if compressed.len() > LARGE_RESPONSE_THRESHOLD {
            if let Some(offloaded) = offload_large_response(&compressed, cmd) {
                return offloaded;
            }
        }
        return compressed;
    }

    let registry = crate::filters::load_filter_registry(workspace);
    let (compressed, ratio) = smedja_adapter::compress_command_output_with(&registry, cmd, &result);

    // No reduction → offload if large, otherwise return verbatim.
    if ratio >= 1.0 {
        if result.len() > LARGE_RESPONSE_THRESHOLD {
            if let Some(offloaded) = offload_large_response(&result, cmd) {
                return offloaded;
            }
        }
        return result;
    }

    // Tee the full uncompressed output to the vault recovery namespace and the
    // in-memory retrieve store, addressed by content hash.
    let hash = content_hash(&result);
    {
        let mut store = retrieve_store().lock().await;
        let mut order = retrieve_store_order().lock().await;
        store.insert(hash.clone(), result.clone());
        order.push_back(hash.clone());
        if store.len() > 512 {
            if let Some(oldest) = order.pop_front() {
                store.remove(&oldest);
            }
        }
    }
    tee_to_vault(&hash, &result, vault).await;

    // Record tokens saved (clamped ≥ 0), separate from billed cost.
    record_tokens_saved(cmd, &result, &compressed, "filter", session, ingot).await;

    // Append the recovery marker naming the hash so the agent can expand it.
    format!("{compressed}\n[smedja_retrieve hash={hash} to expand full output]")
}

/// Tees `full_output` to the vault recovery namespace under `hash`.
///
/// Vault writes are synchronous `SQLite` work; they run on a blocking thread so
/// the async runtime is never blocked. A vault error is logged and swallowed —
/// recovery is best-effort and must never break the tool path.
async fn tee_to_vault(hash: &str, full_output: &str, vault: &Arc<Mutex<Vault>>) {
    let vault = Arc::clone(vault);
    let entry = VaultEntry {
        id: hash.to_owned(),
        embedding: Vec::new(),
        payload: serde_json::json!({ "kind": "filter-recovery" }),
        namespace: FILTER_RECOVERY_NAMESPACE.to_owned(),
        content: full_output.to_owned(),
        source_file: None,
        added_by: Some("output-filter".to_owned()),
        chunk_index: None,
        parent_id: None,
        created_at: 0.0,
        // Recovery rows hold the raw output for hash retrieval, never a semantic
        // embedding, so they carry an empty vector tagged dim 0.
        embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
        dim: 0,
    };
    let join = tokio::task::spawn_blocking(move || {
        let mut guard = vault.blocking_lock();
        guard.upsert(&entry)
    })
    .await;
    match join {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "filter recovery vault tee failed; continuing"),
        Err(e) => tracing::warn!(error = %e, "filter recovery vault tee task panicked; continuing"),
    }
}

/// Records the tokens saved by filtering on the tokens-saved ledger.
///
/// `saved = estimate_tokens(original) - estimate_tokens(compressed)`, clamped at
/// 0. Recorded only when positive, keyed by session (turn `0` at the executor
/// layer, which does not thread a turn index). A ledger error is logged and
/// swallowed — accounting is advisory and must never break the tool path.
async fn record_tokens_saved(
    cmd: &str,
    original: &str,
    compressed: &str,
    source: &str,
    session: Option<&Session>,
    ingot: &IngotHandle,
) {
    let before = smedja_memory::estimate_tokens(original);
    let after = smedja_memory::estimate_tokens(compressed);
    let saved = before.saturating_sub(after);
    if saved == 0 {
        return;
    }
    let Some(session) = session else {
        return;
    };
    let entry = smedja_ingot::TokensSavedEntry {
        id: Uuid::new_v4(),
        session_id: session.id.to_string(),
        turn_n: 0,
        command: cmd.to_owned(),
        tokens_saved: i64::try_from(saved).unwrap_or(i64::MAX),
        source: source.to_owned(),
        created_at: smedja_types::Timestamp::from_micros(0),
    };
    if let Err(e) = ingot.insert_tokens_saved(entry).await {
        tracing::warn!(error = %e, "failed to record tokens-saved; continuing");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{content_hash, filter_command_output, ENV_LOCK};
    use crate::executor::{execute_tool, FILTER_RECOVERY_NAMESPACE, LARGE_RESPONSE_THRESHOLD};

    /// Default FNV embedder for tests that drive `execute_tool`.
    fn test_embedder() -> Arc<dyn crate::embedder_port::Embedder> {
        Arc::new(crate::embedder_port::FnvEmbedder::new())
    }

    fn filter_session() -> smedja_ingot::Session {
        smedja_ingot::Session {
            id: uuid::Uuid::new_v4(),
            created_at: smedja_types::Timestamp::from_micros(0),
            updated_at: smedja_types::Timestamp::from_micros(0),
            status: "active".to_owned(),
            task_id: None,
            mode: None,
            title: String::new(),
            cowork_mode: false,
            workspace_root: None,
            model_override: None,
            runner_override: None,
        }
    }

    // ── output-filters: filter_command_output ─────────────────────────────────

    #[tokio::test]
    async fn known_command_output_is_compressed_on_return_path() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();

        // cargo-build-like noise: many progress lines, one real error.
        let raw = format!(
            "{}error[E0308]: mismatched types\n  --> src/lib.rs:1:1\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let out = filter_command_output(
            "cargo build",
            raw.clone(),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        assert!(
            out.contains("error[E0308]"),
            "the real error must survive filtering; got:\n{out}"
        );
        assert!(
            !out.contains("Compiling"),
            "progress noise must be filtered; got:\n{out}"
        );
        assert!(
            out.len() < raw.len(),
            "filtered output must be shorter than the original"
        );
    }

    #[tokio::test]
    async fn filtered_command_writes_filter_tagged_row() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();
        let session_id = session.id.to_string();

        let raw = format!(
            "{}error[E0308]: mismatched types\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let _ = filter_command_output(
            "cargo build",
            raw,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        assert_eq!(by_source.len(), 1, "exactly one source recorded");
        assert_eq!(by_source[0].0, "filter", "filter path tags source=filter");
        assert!(by_source[0].1 > 0, "a positive saving must be recorded");
    }

    #[tokio::test]
    async fn crusher_json_path_writes_crusher_tagged_row() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();
        let session_id = session.id.to_string();

        // A JSON result with null fields routes through SmartCrusher, which strips
        // them, yielding a positive estimated saving tagged source=crusher.
        let raw = serde_json::json!({
            "a": 1, "b": null, "c": null, "d": null, "e": null,
            "nested": { "x": null, "y": null, "z": "keep" }
        })
        .to_string();
        let _ = filter_command_output("some_tool", raw, ws.path(), Some(&session), &ingot, &vault)
            .await;

        let by_source = ingot
            .session_tokens_saved_by_source(&session_id)
            .await
            .unwrap();
        assert_eq!(by_source.len(), 1, "exactly one source recorded");
        assert_eq!(by_source[0].0, "crusher", "JSON path tags source=crusher");
    }

    #[tokio::test]
    async fn unknown_command_output_passes_through_blank_removal_only() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // An unknown command with no blank lines is returned verbatim (ratio 1.0),
        // preserving the captured success/failure contract.
        let raw = "alpha\nbeta\ngamma".to_owned();
        let out = filter_command_output(
            "some-unknown-tool --flag",
            raw.clone(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;
        assert_eq!(out, raw, "unchanged content must pass through verbatim");
    }

    #[tokio::test]
    async fn error_prefix_contract_is_preserved_through_filtering() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // A failed command's `error:` prefix (the success/failure contract) must
        // never be stripped by filtering.
        let raw = "error: command failed with exit status 1".to_owned();
        let out = filter_command_output(
            "some-unknown-tool",
            raw.clone(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;
        assert!(
            out.starts_with("error:"),
            "the error contract must be preserved; got: {out}"
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // test-only: serialises a process-global env var across the await
    async fn bypass_env_skips_executor_filtering() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        // Serialise env-var mutation so concurrent tests do not race on the
        // process-global SMEDJA_NO_TOOL_COMPRESS bypass.
        let _env_guard = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        std::env::set_var("SMEDJA_NO_TOOL_COMPRESS", "1");
        let raw = format!(
            "{}error[E0308]: mismatched\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let out =
            filter_command_output("cargo build", raw.clone(), ws.path(), None, &ingot, &vault)
                .await;
        std::env::remove_var("SMEDJA_NO_TOOL_COMPRESS");
        assert_eq!(out, raw, "bypass must return the output verbatim");
    }

    // ── output-filters: tee-to-vault recovery ─────────────────────────────────

    #[tokio::test]
    async fn reduced_output_is_teed_and_recoverable_via_marker_hash() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();

        let raw = format!(
            "{}error[E0001]: boom\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let out = filter_command_output(
            "cargo build",
            raw.clone(),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        // The compressed result carries a recovery marker naming the hash.
        let hash = content_hash(&raw);
        assert!(
            out.contains(&format!("smedja_retrieve hash={hash}")),
            "the recovery marker must name the content hash; got:\n{out}"
        );

        // The full output is recoverable via smedja_retrieve (the in-memory store).
        let recovered = execute_tool(
            "smedja_retrieve",
            &format!("{{\"hash\":\"{hash}\"}}"),
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert_eq!(
            recovered, raw,
            "smedja_retrieve must return the full uncompressed output"
        );

        // And the full output is teed to the vault recovery namespace.
        let count = {
            let guard = vault.lock().await;
            guard.count_by_namespace(FILTER_RECOVERY_NAMESPACE).unwrap()
        };
        assert_eq!(count, 1, "full output must be teed to the vault");
    }

    // ── output-filters: savings accounting ────────────────────────────────────

    #[tokio::test]
    async fn filtering_records_positive_tokens_saved() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let session = filter_session();
        let sid = session.id.to_string();

        let raw = format!(
            "{}error[E0001]: boom\n",
            "   Compiling crate v0.1.0\n".repeat(40)
        );
        let _ = filter_command_output(
            "cargo build",
            raw,
            ws.path(),
            Some(&session),
            &ingot,
            &vault,
        )
        .await;

        let saved = ingot.session_tokens_saved(&sid).await.unwrap();
        assert!(
            saved > 0,
            "a filtered command must contribute a positive tokens-saved figure; got {saved}"
        );
    }

    #[tokio::test]
    async fn execute_tool_bash_path_runs_filter_and_preserves_output() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();
        let ws = ws.path().canonicalize().unwrap();

        // An unknown command emitting distinct non-blank lines passes through
        // unchanged (ratio 1.0), confirming the wiring does not corrupt output.
        let out = execute_tool(
            "bash",
            r#"{"command":"printf 'one\ntwo\nthree\n'"}"#,
            &ws,
            None,
            &ingot,
            &vault,
            &test_embedder(),
        )
        .await;
        assert!(
            out.contains("one"),
            "command output must survive; got: {out}"
        );
        assert!(
            out.contains("three"),
            "command output must survive; got: {out}"
        );
    }

    // ── large response offload ────────────────────────────────────────────────

    #[tokio::test]
    async fn large_text_output_is_offloaded_rather_than_returned_verbatim() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        // Generate a text result that exceeds the threshold and won't be
        // compressed by the filter registry (not a known command output pattern).
        let large = "z".repeat(LARGE_RESPONSE_THRESHOLD + 1);

        let out = filter_command_output(
            "unknown_cmd",
            large.clone(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;

        assert!(
            out.len() < large.len(),
            "offloaded output must be shorter than original ({} bytes returned)",
            out.len()
        );
        assert!(
            out.contains("bytes"),
            "reference must mention byte count: {out}"
        );
    }

    #[tokio::test]
    async fn small_text_output_below_threshold_is_returned_verbatim() {
        use smedja_ingot::{Ingot, IngotHandle};
        use smedja_vault::Vault;
        use tokio::sync::Mutex;

        let ingot = IngotHandle::new(Ingot::open_in_memory().unwrap());
        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let ws = tempfile::tempdir().unwrap();

        let small = "hello world";

        let out = filter_command_output(
            "unknown_cmd",
            small.to_owned(),
            ws.path(),
            None,
            &ingot,
            &vault,
        )
        .await;

        assert_eq!(out, small, "small output must pass through unchanged");
    }
}
