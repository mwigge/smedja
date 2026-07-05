use super::*;
use std::collections::HashMap;

use smedja_adapter::{GpuSnapshot, LocalModel};
use smedja_assayer::{Runner, Tier};

#[test]
fn model_default_uses_builtin_then_env_override() {
    // Use a unique runner key so the env var can't collide with a real one.
    let key = "SMEDJA_MODEL_ZZTEST_DEEP";
    std::env::remove_var(key);
    assert_eq!(
        model_default("zztest-cli", Tier::Deep, "builtin-x"),
        "builtin-x",
        "falls back to the built-in when unset"
    );
    std::env::set_var(key, "  ");
    assert_eq!(
        model_default("zztest", Tier::Deep, "builtin-x"),
        "builtin-x",
        "blank env override is ignored"
    );
    std::env::set_var(key, "future-model-9");
    assert_eq!(
        model_default("zztest-cli", Tier::Deep, "builtin-x"),
        "future-model-9",
        "env override wins so new models need no recompile"
    );
    std::env::remove_var(key);
}

struct NullProvider;
impl smedja_adapter::Provider for NullProvider {
    fn stream_chat(
        &self,
        _messages: &[smedja_adapter::Message],
        _opts: &smedja_adapter::CallOptions,
    ) -> smedja_adapter::DeltaStream {
        Box::pin(futures_util::stream::empty())
    }
}

fn pool_with(entries: Vec<((Runner, Tier), &'static str, &'static str)>) -> ProviderPool {
    let mut map = HashMap::new();
    let mut order = Vec::new();
    let mut default = None;
    for (key, runner_name, default_model) in entries {
        if default.is_none() {
            default = Some(key);
        }
        if map
            .insert(
                key,
                ProviderEntry {
                    provider: Box::new(NullProvider),
                    runner: key.0,
                    tier: key.1,
                    runner_name,
                    default_model: default_model.to_owned(),
                },
            )
            .is_none()
        {
            order.push(key);
        }
    }
    ProviderPool {
        entries: map,
        order,
        default,
        local: None,
    }
}

#[test]
fn local_control_exposes_inventory_and_mutable_active_model() {
    let control = LocalControl::new(
        "http://127.0.0.1:9090".to_owned(),
        "http://127.0.0.1:9090".to_owned(),
        vec![
            LocalModel {
                id: "qwen3-14b".to_owned(),
                est_vram_mb: Some(9000),
            },
            LocalModel {
                id: "llama3-8b".to_owned(),
                est_vram_mb: None,
            },
        ],
        GpuSnapshot::none(),
        Some("qwen3-14b".to_owned()),
    );
    let pool = ProviderPool {
        entries: HashMap::new(),
        order: Vec::new(),
        default: None,
        local: Some(control),
    };

    let local = pool.local_control().expect("local control present");
    let ids: Vec<&str> = local.inventory.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["qwen3-14b", "llama3-8b"],
        "pool entry must expose the full local inventory"
    );
    assert_eq!(local.active_model_id().as_deref(), Some("qwen3-14b"));

    // The active model must be mutable in place without rebuilding the pool.
    let previous = local.set_active_model_id("llama3-8b");
    assert_eq!(previous.as_deref(), Some("qwen3-14b"));
    assert_eq!(local.active_model_id().as_deref(), Some("llama3-8b"));
}

#[test]
fn berget_and_local_coexist_under_distinct_keys() {
    // With Berget keyed under its own runner, a healthy local endpoint and
    // Berget both live in the pool and each resolves to its own provider.
    let pool = pool_with(vec![
        ((Runner::Local, Tier::Local), "local", "local"),
        ((Runner::Berget, Tier::Local), "berget", "gpt-4o-mini"),
    ]);
    assert_eq!(
        pool.get(Runner::Local, Tier::Local).map(|e| e.runner_name),
        Some("local"),
    );
    assert_eq!(
        pool.get(Runner::Berget, Tier::Local).map(|e| e.runner_name),
        Some("berget"),
        "Berget stays routable alongside a healthy local endpoint",
    );

    // Contrast — the pre-fix keying put both under (Local, Local), which
    // collapses to a single entry so whichever probed second silently wins.
    let collided = pool_with(vec![
        ((Runner::Local, Tier::Local), "local", "local"),
        ((Runner::Local, Tier::Local), "berget", "gpt-4o-mini"),
    ]);
    assert_eq!(
        collided.list_all_entries().len(),
        1,
        "a shared key drops one provider",
    );
}

#[test]
fn get_returns_exact_match() {
    let pool = pool_with(vec![
        (
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        ),
        ((Runner::Local, Tier::Local), "local", "local"),
    ]);
    let entry = pool.get(Runner::Local, Tier::Local).unwrap();
    assert_eq!(entry.runner_name, "local");
}

#[test]
fn get_falls_back_to_default() {
    let pool = pool_with(vec![(
        (Runner::Claude, Tier::Fast),
        "claude-cli",
        "claude-haiku-4-5-20251001",
    )]);
    // Codex is not in the pool; should fall back to default (claude-cli).
    let entry = pool.get(Runner::Codex, Tier::Fast).unwrap();
    assert_eq!(entry.runner_name, "claude-cli");
}

#[test]
fn get_returns_none_on_empty_pool() {
    let pool = ProviderPool {
        entries: HashMap::new(),
        order: Vec::new(),
        default: None,
        local: None,
    };
    assert!(pool.get(Runner::Claude, Tier::Fast).is_none());
}

#[test]
fn empty_pool_reports_is_empty() {
    let pool = ProviderPool {
        entries: std::collections::HashMap::new(),
        order: Vec::new(),
        default: None,
        local: None,
    };
    assert!(
        pool.is_empty(),
        "a pool with no providers must report empty"
    );
    assert_eq!(pool.default_runner_name(), "unknown");
}

#[test]
fn default_runner_name_returns_first_inserted() {
    let pool = pool_with(vec![
        (
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        ),
        ((Runner::Local, Tier::Local), "local", "local"),
    ]);
    assert_eq!(pool.default_runner_name(), "claude-cli");
}

#[test]
fn available_runners_lists_unique_names() {
    let pool = pool_with(vec![
        (
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        ),
        (
            (Runner::Claude, Tier::Deep),
            "claude-cli",
            "claude-sonnet-4-6",
        ),
        ((Runner::Local, Tier::Local), "local", "local"),
    ]);
    let mut runners = pool.available_runners();
    runners.sort_unstable();
    assert_eq!(runners, vec!["claude-cli", "local"]);
}

#[test]
fn pool_with_only_local_provider_returns_local() {
    let pool = pool_with(vec![((Runner::Local, Tier::Local), "local", "local")]);
    // All routes fall back to local.
    let entry = pool.get(Runner::Claude, Tier::Deep).unwrap();
    assert_eq!(entry.runner_name, "local");
}

#[test]
fn list_all_entries_returns_runner_tier_model_triples() {
    let pool = pool_with(vec![
        (
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        ),
        (
            (Runner::Claude, Tier::Deep),
            "claude-cli",
            "claude-sonnet-4-6",
        ),
        ((Runner::Local, Tier::Local), "local", "qwen3-14b"),
    ]);
    let entries = pool.list_all_entries();
    assert_eq!(entries.len(), 3);
    let tiers: Vec<&str> = entries.iter().map(|&(_, t, _)| t).collect();
    assert!(
        tiers.contains(&"fast"),
        "fast tier must appear in list_all_entries"
    );
    assert!(
        tiers.contains(&"deep"),
        "deep tier must appear in list_all_entries"
    );
    assert!(
        tiers.contains(&"local"),
        "local tier must appear in list_all_entries"
    );
}

#[test]
fn eligible_ring_orders_routed_first_then_compatible_dedup() {
    // Insertion/priority order: claude-fast (default), claude-deep, local.
    let pool = pool_with(vec![
        (
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        ),
        (
            (Runner::Claude, Tier::Deep),
            "claude-cli",
            "claude-sonnet-4-6",
        ),
        ((Runner::Local, Tier::Local), "local", "local"),
    ]);

    // Route to fast: ring starts with the routed fast entry, then the more
    // capable local and deep entries in priority order, ending with the
    // default (already yielded → not duplicated).
    let ring = pool.eligible_ring(Runner::Claude, Tier::Fast);
    let names: Vec<&str> = ring.iter().map(|e| e.runner_name).collect();
    assert_eq!(
        names,
        vec!["claude-cli", "claude-cli", "local"],
        "ring must be routed-first then compatible entries in priority order"
    );

    // Every (Runner, Tier) appears at most once: ring length never exceeds
    // the number of distinct entries.
    assert_eq!(ring.len(), 3, "ring must de-duplicate by (Runner, Tier)");
}

#[test]
fn deep_route_does_not_rotate_down_to_fast() {
    assert!(
        !tier_compatible(Tier::Deep, Tier::Fast, "rate_limited"),
        "a deep-routed turn must not rotate down to fast"
    );
    assert!(
        tier_compatible(Tier::Deep, Tier::Deep, "rate_limited"),
        "deep is compatible with itself"
    );
    assert!(
        tier_compatible(Tier::Fast, Tier::Deep, "rate_limited"),
        "fast may rotate up to deep"
    );
    assert!(
        tier_compatible(Tier::Fast, Tier::Local, "rate_limited"),
        "fast may rotate up to local"
    );
}

#[test]
fn context_length_kind_requires_more_capable_tier() {
    assert!(
        !tier_compatible(Tier::Deep, Tier::Deep, "context_length_exceeded"),
        "context-length must not rotate to an equal-window tier"
    );
    assert!(
        tier_compatible(Tier::Fast, Tier::Deep, "context_length_exceeded"),
        "context-length may rotate to a strictly-more-capable tier"
    );
    assert!(
        !tier_compatible(Tier::Local, Tier::Fast, "context_length_exceeded"),
        "context-length must not rotate down"
    );
    assert!(
        tier_compatible(Tier::Local, Tier::Deep, "context_length_exceeded"),
        "local may rotate up to deep on context-length"
    );
}

#[test]
fn eligible_ring_routes_deep_excludes_fast_entries() {
    let pool = pool_with(vec![
        (
            (Runner::Claude, Tier::Fast),
            "claude-cli",
            "claude-haiku-4-5-20251001",
        ),
        (
            (Runner::Claude, Tier::Deep),
            "claude-cli",
            "claude-sonnet-4-6",
        ),
    ]);
    let ring = pool.eligible_ring(Runner::Claude, Tier::Deep);
    // Routed deep entry only; the fast entry is less capable and excluded.
    // The default (claude-fast) is incompatible so it is not appended.
    assert_eq!(ring.len(), 1, "deep route must exclude the fast entry");
}

#[test]
fn native_api_preferred_over_subprocess_for_claude() {
    assert_eq!(
        claude_preferred_runner(true, true),
        Some("anthropic"),
        "API key wins over binary"
    );
    assert_eq!(
        claude_preferred_runner(false, true),
        Some("claude-cli"),
        "binary used when no key"
    );
    assert_eq!(
        claude_preferred_runner(true, false),
        Some("anthropic"),
        "key works without binary"
    );
    assert_eq!(claude_preferred_runner(false, false), None);
}

#[test]
fn native_api_preferred_over_subprocess_for_codex() {
    assert_eq!(
        codex_preferred_runner(true, true),
        Some("openai"),
        "API key wins over binary"
    );
    assert_eq!(
        codex_preferred_runner(false, true),
        Some("codex-cli"),
        "binary used when no key"
    );
    assert_eq!(
        codex_preferred_runner(true, false),
        Some("openai"),
        "key works without binary"
    );
    assert_eq!(codex_preferred_runner(false, false), None);
}
