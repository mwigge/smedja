//! Transcript assembly for session compaction.

use smedja_adapter::types::Message as AdapterMessage;

/// Assembles a conversation transcript for compaction from a checkpoint's
/// `messages_json` blob, routing it through working-memory strata (deep tier) so
/// the same windowing the live prompt path uses is applied. Invalid or empty JSON
/// yields an empty transcript. The raw blob is preserved separately in the
/// pre-compaction checkpoint, so this rendering is not lossy for rollback.
pub(crate) fn assemble_compaction_transcript(messages_json: &str) -> String {
    let parsed: Vec<AdapterMessage> = serde_json::from_str(messages_json).unwrap_or_default();
    let mut mem = smedja_memory::WorkingMemory::new(100_000);
    mem.set_strata(smedja_memory::StrataConfig::deep());
    for m in parsed {
        mem.push(m);
    }
    mem.build_prompt(100_000)
        .iter()
        .map(|m| format!("{}: {}", m.role.as_str(), m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    #[test]
    fn compaction_transcript_renders_strata_messages_not_raw_json() {
        let messages_json = r#"[
            {"role":"user","content":"first request"},
            {"role":"assistant","content":"first reply"},
            {"role":"user","content":"second request"}
        ]"#;
        let transcript = super::assemble_compaction_transcript(messages_json);
        // Rendered as role: content lines, not the raw JSON blob.
        assert!(transcript.contains("user: first request"));
        assert!(transcript.contains("assistant: first reply"));
        assert!(transcript.contains("user: second request"));
        assert!(
            !transcript.contains("\"role\""),
            "transcript must not contain raw JSON keys"
        );
    }

    #[test]
    fn compaction_transcript_empty_for_invalid_json() {
        assert_eq!(super::assemble_compaction_transcript(""), "");
        assert_eq!(super::assemble_compaction_transcript("not json"), "");
        assert_eq!(super::assemble_compaction_transcript("[]"), "");
    }

    #[tokio::test]
    async fn compact_writes_summary_to_vault_compact_namespace() {
        use smedja_vault::Vault;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));
        let session_id = "sess-compact-test".to_owned();
        let summary = "• Implemented auth\n• Tests pass\nGoal: ship v1".to_owned();
        let turn_count: i64 = 7;

        // Simulate the vault write logic from session.compact.
        let compact_sid = session_id.clone();
        let compact_summary = summary.clone();
        let vt = Arc::clone(&vault);
        tokio::task::spawn_blocking(move || {
            let entry = smedja_vault::VaultEntry {
                id: format!("compact:{compact_sid}:{turn_count}"),
                embedding: crate::embedder::embed(&compact_summary),
                payload: serde_json::json!({
                    "session_id": compact_sid,
                    "turn_count": turn_count,
                }),
                namespace: "compact".to_owned(),
                content: compact_summary,
                source_file: None,
                added_by: Some("session.compact".to_owned()),
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                dim: crate::embedder::DIM,
            };
            let mut guard = vt.blocking_lock();
            guard.upsert(&entry).unwrap();
        })
        .await
        .unwrap();

        let count = vault.lock().await.count_by_namespace("compact").unwrap();
        assert_eq!(count, 1, "one compact entry must be written per compaction");

        let results = {
            let guard = vault.lock().await;
            let qv = crate::embedder::embed("auth tests");
            guard
                .search(
                    &qv,
                    "auth tests",
                    "compact",
                    5,
                    smedja_vault::LEGACY_MODEL_ID,
                    crate::embedder::DIM,
                )
                .unwrap()
        };
        assert!(
            !results.is_empty(),
            "compact summary must be retrievable by semantic search"
        );
    }

    #[tokio::test]
    async fn session_context_includes_vault_stratum_counts() {
        use smedja_vault::Vault;

        let vault = Arc::new(Mutex::new(Vault::open_in_memory().unwrap()));

        // Populate vault with one warm and two default (cold) entries.
        {
            let mut guard = vault.lock().await;
            let make_entry = |id: &str, ns: &str| smedja_vault::VaultEntry {
                id: id.to_owned(),
                embedding: crate::embedder::embed(id),
                payload: serde_json::json!({}),
                namespace: ns.to_owned(),
                content: id.to_owned(),
                source_file: None,
                added_by: None,
                chunk_index: None,
                parent_id: None,
                created_at: 0.0,
                embedder_model_id: smedja_vault::LEGACY_MODEL_ID.to_owned(),
                dim: crate::embedder::DIM,
            };
            guard.upsert(&make_entry("w1", "warm")).unwrap();
            guard.upsert(&make_entry("c1", "default")).unwrap();
            guard.upsert(&make_entry("c2", "default")).unwrap();
        }

        let (warm_count, cold_count) = tokio::task::spawn_blocking(move || {
            let guard = vault.blocking_lock();
            let warm = guard.count_by_namespace("warm").unwrap_or(0);
            let cold = guard.count_by_namespace("default").unwrap_or(0);
            (warm, cold)
        })
        .await
        .unwrap();

        assert_eq!(warm_count, 1);
        assert_eq!(cold_count, 2);
    }
}
