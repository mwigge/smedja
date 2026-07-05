use std::path::Path;
use std::time::Duration;

use super::*;

fn make_ctx() -> ModuleContext {
    ModuleContext {
        tier: None,
        model: None,
        context_used: 0,
        context_window: 0,
        active_task: None,
        last_exit_code: None,
        input_tokens: None,
        output_tokens: None,
        latency_ms: None,
        traceparent: None,
        session_id: None,
        cwd: None,
        interface: None,
        tokens_saved: None,
        efficiency_ratio: None,
    }
}

// 1
#[test]
fn tier_module_shows_local() {
    let ctx = ModuleContext {
        tier: Some("local".to_owned()),
        ..make_ctx()
    };
    let seg = TierModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "[local]");
}

// 2
#[test]
fn tier_module_missing_ctx_returns_none() {
    let ctx = make_ctx();
    assert!(TierModule.evaluate(&ctx).is_none());
}

#[test]
fn tier_module_colours_by_tier() {
    for (tier, want) in [
        ("local", &FORGE_LOCAL),
        ("fast", &FORGE_FAST),
        ("deep", &FORGE_DEEP),
    ] {
        let ctx = ModuleContext {
            tier: Some(tier.to_owned()),
            ..make_ctx()
        };
        let seg = TierModule.evaluate(&ctx).expect("should return Some");
        assert_eq!(seg.text, format!("[{tier}]"));
        assert_eq!(seg.style.fg.as_ref(), Some(want), "tier {tier} colour");
    }
}

#[test]
fn tier_module_unknown_tier_stays_plain() {
    let ctx = ModuleContext {
        tier: Some("weird".to_owned()),
        ..make_ctx()
    };
    let seg = TierModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "[weird]");
    assert!(seg.style.fg.is_none(), "unknown tier must stay uncoloured");
}

#[test]
fn model_context_window_matches_published_maximums() {
    assert_eq!(model_context_window("claude-opus-4-8"), 200_000);
    assert_eq!(model_context_window("Claude-3.5-Sonnet"), 200_000);
    assert_eq!(model_context_window("haiku"), 200_000);
    assert_eq!(model_context_window("gpt-4o-mini"), 128_000);
    assert_eq!(model_context_window("gpt-4.1"), 128_000);
    assert_eq!(model_context_window("o3-mini"), 128_000);
    assert_eq!(model_context_window("gemini-1.5-pro"), 1_000_000);
    assert_eq!(model_context_window("gemma-4-27b-it"), 8_192);
    assert_eq!(model_context_window("llama-3.1-70b"), 128_000);
    assert_eq!(model_context_window("qwen2.5-coder"), 32_768);
    // Unknown families fall back to the conservative 128 K default.
    assert_eq!(model_context_window("some-unknown-model"), 128_000);
}

// 3
#[test]
fn context_pct_module_colours_by_threshold() {
    // 50 % → green
    let ctx_50 = ModuleContext {
        context_used: 50,
        context_window: 100,
        ..make_ctx()
    };
    let seg = ContextPctModule
        .evaluate(&ctx_50)
        .expect("50% should return Some");
    let fg = seg.style.fg.as_ref().expect("should have fg colour");
    assert_eq!(*fg, FORGE_SUCCESS, "50% should be forge green");

    // 70 % → forge amber
    let ctx_70 = ModuleContext {
        context_used: 70,
        context_window: 100,
        ..make_ctx()
    };
    let seg = ContextPctModule
        .evaluate(&ctx_70)
        .expect("70% should return Some");
    let fg = seg.style.fg.as_ref().expect("should have fg colour");
    assert_eq!(*fg, FORGE_WARN, "70% should be forge amber");

    // 90 % → forge ember
    let ctx_90 = ModuleContext {
        context_used: 90,
        context_window: 100,
        ..make_ctx()
    };
    let seg = ContextPctModule
        .evaluate(&ctx_90)
        .expect("90% should return Some");
    let fg = seg.style.fg.as_ref().expect("should have fg colour");
    assert_eq!(*fg, FORGE_ERROR, "90% should be forge ember");
}

// 4
#[test]
fn time_module_returns_hh_mm_format() {
    let ctx = make_ctx();
    let seg = TimeModule
        .evaluate(&ctx)
        .expect("TimeModule always returns Some");
    let text = &seg.text;
    assert_eq!(text.len(), 5, "expected HH:MM (5 chars), got '{text}'");
    assert_eq!(
        text.chars().nth(2),
        Some(':'),
        "colon must be at position 2"
    );
    for (i, ch) in text.chars().enumerate() {
        if i != 2 {
            assert!(
                ch.is_ascii_digit(),
                "char at {i} must be a digit, got '{ch}'"
            );
        }
    }
}

// 5
#[test]
fn git_branch_module_not_in_repo_returns_none() {
    // Evaluate against /tmp which is guaranteed not to be inside a git repo.
    let result = GitBranchModule::default().evaluate_in(&make_ctx(), Path::new("/tmp"));
    assert!(
        result.is_none(),
        "expected None for non-git directory, got {result:?}"
    );
}

// 7
#[test]
fn format_bar_replaces_module_tokens() {
    let segs = vec![Segment {
        name: "tier".to_owned(),
        text: "[local]".to_owned(),
        style: SegmentStyle::default(),
    }];
    let result = format_bar(&segs, "$tier active");
    assert_eq!(result, "[local] active");
}

// 8
#[test]
fn format_bar_separator_becomes_dim_char() {
    let result = format_bar(&[], "a | b");
    assert!(
        result.contains('\u{2502}'),
        "expected box-drawing │, got '{result}'"
    );
}

// 10
#[test]
fn parallel_render_collects_all_segments() {
    let ctx = ModuleContext {
        tier: Some("local".to_owned()),
        model: Some("gemma-4-27b".to_owned()),
        ..make_ctx()
    };
    let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(TierModule), Box::new(ModelModule)];
    let segments = render_status_bar_parallel(&modules, &ctx, 500);
    assert!(
        !segments.is_empty(),
        "expected at least one segment, got {}",
        segments.len()
    );
    assert!(
        segments.iter().any(|s| s.text == "[local]"),
        "expected [local] segment in {segments:?}"
    );
}

// 12
#[test]
fn exit_code_module_zero_returns_none() {
    let ctx = ModuleContext {
        last_exit_code: Some(0),
        ..make_ctx()
    };
    assert!(ExitCodeModule.evaluate(&ctx).is_none());
}

// 13
#[test]
fn exit_code_module_nonzero_returns_red_segment() {
    let ctx = ModuleContext {
        last_exit_code: Some(1),
        ..make_ctx()
    };
    let seg = ExitCodeModule
        .evaluate(&ctx)
        .expect("should return Some for exit 1");
    assert!(seg.text.contains('1'), "text should include exit code");
    assert!(seg.text.contains('\u{2718}'), "text should contain ✘");
    let fg = seg.style.fg.as_ref().expect("should have fg colour");
    assert_eq!(*fg, FORGE_ERROR, "non-zero exit should use forge ember");
}

// 14
#[test]
fn exit_code_module_absent_returns_none() {
    assert!(ExitCodeModule.evaluate(&make_ctx()).is_none());
}

// 15
#[test]
fn git_branch_module_with_symbol_uses_symbol() {
    let module = GitBranchModule::with_symbol(Some(" ".to_owned()));
    // Evaluate against the smedja repo itself — must be on a branch.
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    if let Some(seg) = module.evaluate_in(&make_ctx(), repo_root) {
        assert!(
            seg.text.starts_with(' '),
            "expected segment to start with symbol, got '{}'",
            seg.text
        );
    }
}

// 16
#[test]
fn git_branch_module_default_uses_asterisk() {
    let module = GitBranchModule::default();
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    if let Some(seg) = module.evaluate_in(&make_ctx(), repo_root) {
        assert!(
            seg.text.starts_with("* "),
            "expected segment to start with '* ', got '{}'",
            seg.text
        );
    }
}

// 17
#[test]
fn tokens_module_formats_up_down_arrows() {
    let ctx = ModuleContext {
        input_tokens: Some(412),
        output_tokens: Some(88),
        ..make_ctx()
    };
    let seg = TokensModule.evaluate(&ctx).expect("should return Some");
    assert!(
        seg.text.contains("412"),
        "expected input count in '{}'",
        seg.text
    );
    assert!(
        seg.text.contains("88"),
        "expected output count in '{}'",
        seg.text
    );
    assert!(
        seg.text.contains('\u{2191}'),
        "expected ↑ in '{}'",
        seg.text
    );
    assert!(
        seg.text.contains('\u{2193}'),
        "expected ↓ in '{}'",
        seg.text
    );
}

// 18
#[test]
fn tokens_module_none_when_missing() {
    assert!(TokensModule.evaluate(&make_ctx()).is_none());
    let ctx = ModuleContext {
        input_tokens: Some(10),
        ..make_ctx()
    };
    assert!(
        TokensModule.evaluate(&ctx).is_none(),
        "missing output_tokens must return None"
    );
}

// 19
#[test]
fn latency_module_sub_second_shows_ms() {
    let ctx = ModuleContext {
        latency_ms: Some(800),
        ..make_ctx()
    };
    let seg = LatencyModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "800ms");
}

// 20
#[test]
fn latency_module_multi_second_shows_decimal_s() {
    let ctx = ModuleContext {
        latency_ms: Some(4200),
        ..make_ctx()
    };
    let seg = LatencyModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "4.2s");
}

// 21
#[test]
fn latency_module_none_when_missing() {
    assert!(LatencyModule.evaluate(&make_ctx()).is_none());
}

#[test]
fn efficiency_module_renders_ratio_as_percentage() {
    let ctx = ModuleContext {
        efficiency_ratio: Some(0.41),
        ..make_ctx()
    };
    let seg = EfficiencyModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "\u{2b07} 41%");
}

#[test]
fn efficiency_module_falls_back_to_tokens_saved() {
    let ctx = ModuleContext {
        efficiency_ratio: None,
        tokens_saved: Some(2_300_000),
        ..make_ctx()
    };
    let seg = EfficiencyModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "\u{2212}2300000 tok");
}

#[test]
fn efficiency_module_none_when_absent_no_misleading_zero() {
    // Neither figure present → no segment, rather than a misleading 0%.
    assert!(EfficiencyModule.evaluate(&make_ctx()).is_none());
}

// 22
#[test]
fn trace_module_extracts_first_eight_chars_of_trace_id() {
    let ctx = ModuleContext {
        traceparent: Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01".to_owned()),
        ..make_ctx()
    };
    let seg = TraceModule.evaluate(&ctx).expect("should return Some");
    assert_eq!(seg.text, "trace:4bf92f35");
}

// 23
#[test]
fn trace_module_none_when_missing() {
    assert!(TraceModule.evaluate(&make_ctx()).is_none());
}

// 11
#[test]
fn app_name_module_always_returns_smedja() {
    let ctx = make_ctx();
    let seg = AppNameModule
        .evaluate(&ctx)
        .expect("AppNameModule must return Some");
    assert_eq!(seg.text, "smedja");
}

#[test]
fn session_id_module_returns_first_eight_chars() {
    let ctx = ModuleContext {
        session_id: Some("abcdef1234567890".to_owned()),
        ..make_ctx()
    };
    let seg = SessionIdModule
        .evaluate(&ctx)
        .expect("SessionIdModule must return Some");
    assert_eq!(seg.text, "abcdef12");
}

#[test]
fn session_id_module_returns_none_when_absent() {
    let ctx = make_ctx();
    assert!(SessionIdModule.evaluate(&ctx).is_none());
}

#[test]
fn cwd_module_truncates_long_path() {
    let long = "/home/user/very/deep/path/that/exceeds/the/forty/char/limit";
    let ctx = ModuleContext {
        cwd: Some(long.to_owned()),
        ..make_ctx()
    };
    let seg = CwdModule
        .evaluate(&ctx)
        .expect("CwdModule must return Some");
    assert!(
        seg.text.starts_with('\u{2026}'),
        "long cwd must start with ellipsis"
    );
    assert!(
        seg.text.chars().count() <= 41,
        "truncated cwd must be at most 41 chars (ellipsis + 40)"
    );
}

#[test]
fn cwd_module_returns_full_short_path() {
    let ctx = ModuleContext {
        cwd: Some("/home/user".to_owned()),
        ..make_ctx()
    };
    let seg = CwdModule
        .evaluate(&ctx)
        .expect("CwdModule must return Some");
    assert_eq!(seg.text, "/home/user");
}

#[test]
fn module_timeout_emits_question_mark() {
    struct SlowModule;
    impl StatusModule for SlowModule {
        fn name(&self) -> &'static str {
            "slow"
        }
        fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
            std::thread::sleep(Duration::from_millis(200));
            Some(plain_segment("slow", "done"))
        }
        fn timeout_ms(&self) -> u64 {
            10
        }
    }

    let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(SlowModule)];
    let ctx = make_ctx();
    let segments = render_status_bar_parallel(&modules, &ctx, 500);
    assert_eq!(segments.len(), 1, "expected exactly one timeout segment");
    assert_eq!(
        segments[0].text, "?",
        "timed-out module must emit '?' placeholder"
    );
}

#[test]
fn slow_module_is_joined_not_leaked_on_timeout() {
    // Regression for the use-after-free: the old implementation transmuted a
    // borrow of `module` into a detached raw-pointer thread and returned on
    // timeout while that thread could still dereference the (possibly dropped)
    // slice — and leaked one thread per slow module. The scoped-thread fix
    // JOINS the worker before returning, so its evaluate() must have finished
    // by the time the render call returns.
    use std::sync::atomic::{AtomicBool, Ordering};
    static FINISHED: AtomicBool = AtomicBool::new(false);

    struct BlockingModule;
    impl StatusModule for BlockingModule {
        fn name(&self) -> &'static str {
            "blocking"
        }
        fn evaluate(&self, _ctx: &ModuleContext) -> Option<Segment> {
            std::thread::sleep(Duration::from_millis(80));
            FINISHED.store(true, Ordering::SeqCst);
            Some(plain_segment("blocking", "done"))
        }
        fn timeout_ms(&self) -> u64 {
            5
        }
    }

    FINISHED.store(false, Ordering::SeqCst);
    let modules: Vec<Box<dyn StatusModule>> = vec![Box::new(BlockingModule)];
    let ctx = make_ctx();
    let segments = render_status_bar_parallel(&modules, &ctx, 500);

    assert_eq!(segments.len(), 1);
    assert_eq!(
        segments[0].text, "?",
        "slow module still yields the placeholder"
    );
    assert!(
        FINISHED.load(Ordering::SeqCst),
        "the scoped worker must have been joined (ran to completion) — no leak, no UAF"
    );
}

#[test]
fn trace_module_multibyte_trace_id_does_not_panic() {
    // trace_id = "中中中中" (4×3 bytes). A raw `&trace_id[..8]` splits the
    // third codepoint (boundaries at 0,3,6,9,12) → panic. Fail-before.
    let ctx = ModuleContext {
        traceparent: Some("00-\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}-b7ad-01".to_owned()),
        ..make_ctx()
    };
    let seg = TraceModule.evaluate(&ctx).expect("must return Some");
    assert_eq!(
        seg.text, "trace:\u{4e2d}\u{4e2d}",
        "floors 8 bytes down to 6"
    );
}

#[test]
fn session_id_module_multibyte_does_not_panic() {
    // session_id starting with 3-byte codepoints; `&sid[..8]` would split
    // the third one. Fail-before: panic.
    let ctx = ModuleContext {
        session_id: Some("\u{4e2d}\u{4e2d}\u{4e2d}\u{4e2d}session".to_owned()),
        ..make_ctx()
    };
    let seg = SessionIdModule.evaluate(&ctx).expect("must return Some");
    assert_eq!(seg.text, "\u{4e2d}\u{4e2d}", "floors 8 bytes down to 6");
}

#[test]
fn cwd_module_multibyte_long_path_does_not_panic() {
    // 41 three-byte codepoints = 123 bytes. The old `&cwd[cwd.len()-40..]`
    // sliced at byte 83, which is mid-codepoint (not a multiple of 3) and
    // panicked. Fail-before.
    let cwd = "\u{20ac}".repeat(41);
    let ctx = ModuleContext {
        cwd: Some(cwd),
        ..make_ctx()
    };
    let seg = CwdModule.evaluate(&ctx).expect("must return Some");
    assert!(
        seg.text.starts_with('\u{2026}'),
        "an over-length path is prefixed with an ellipsis"
    );
    assert_eq!(
        seg.text.chars().count(),
        41,
        "ellipsis + last 40 characters"
    );
    assert!(seg.text.chars().skip(1).all(|c| c == '\u{20ac}'));
}
