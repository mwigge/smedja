//! Tier-2 adversary LLM review for the quality panel.
//!
//! [`quality_reviewer_model`] maps the primary provider to an adversary model
//! from the opposite family, so the review is not self-confirming.
//!
//! [`review_turn`] calls the adversary model with a rubric prompt, parses the
//! JSON response into [`QualityLlmReview`], and falls back gracefully on any
//! adapter or parse error.

use futures_util::StreamExt as _;
use smedja_adapter::{AnthropicProvider, CallOptions, Message, OpenAiProvider, Provider};
use tracing::warn;

/// Rubric prompt template for the adversary LLM reviewer.
const RUBRIC: &str = r#"You are a senior engineering reviewer. Score the following diff on four dimensions (25 pts each, 100 total). Be strict.

Dimensions:
1. TDD — every new function has at least one test.
2. Clean — no raw `.unwrap()` / `.expect()` on the request path; no `println!` in lib code.
3. File size — no file exceeds 600 lines.
4. Skill inject — security/API/DB patterns detected in the diff are paired with the relevant methodology skill invocation.

Respond with ONLY this JSON object:
{"score": <0-100>, "findings": ["<finding1>", ...up to 5], "suggested_command": "<slash-command or null>"}

The diff to review:
```
{diff}
```

Tier-1 deterministic score: {tier1_score}/100."#;

/// Result of a Tier-2 LLM quality review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualityLlmReview {
    /// Composite 0-100 score from the adversary model.
    pub score: u8,
    /// ≤ 5 human-readable finding strings.
    pub findings: Vec<String>,
    /// A concrete slash command the adversary suggests, if any.
    pub suggested_command: Option<String>,
    /// Whether the review was produced by an LLM (false = unavailable fallback).
    pub llm_reviewed: bool,
    /// Input tokens used by the review call (0 on error).
    pub input_tokens: u32,
    /// Output tokens used by the review call (0 on error).
    pub output_tokens: u32,
}

impl QualityLlmReview {
    /// Unavailability fallback: preserves the Tier-1 score and surfaces a
    /// single advisory so the panel communicates the failure clearly.
    #[must_use]
    pub fn unavailable(tier1_score: u8) -> Self {
        Self {
            score: tier1_score,
            findings: vec!["llm unavailable".into()],
            suggested_command: None,
            llm_reviewed: false,
            input_tokens: 0,
            output_tokens: 0,
        }
    }
}

/// Maps the primary provider name to an adversary reviewer model from the
/// opposite provider family so the review is not self-confirming.
///
/// - Primary is `OpenAI`/`Codex` → use Anthropic Haiku
/// - Primary is anything else (Anthropic/Claude/local) → use `OpenAI` `GPT-4o-mini`
#[must_use]
pub fn quality_reviewer_model(primary_provider: &str) -> &'static str {
    let lc = primary_provider.to_ascii_lowercase();
    if lc.contains("openai") || lc.contains("codex") {
        "claude-haiku-4-5-20251001"
    } else {
        "gpt-4o-mini"
    }
}

/// Parses the adversary model's JSON response into a [`QualityLlmReview`].
///
/// Returns `None` if the JSON is missing or malformed; the caller should fall
/// back to [`QualityLlmReview::unavailable`].
#[must_use]
pub fn parse_review_response(
    text: &str,
    input_tokens: u32,
    output_tokens: u32,
) -> Option<QualityLlmReview> {
    // Strip markdown code fences if the model wrapped the JSON.
    let stripped = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    let v: serde_json::Value = serde_json::from_str(stripped).ok()?;
    let score = v.get("score")?.as_u64()?.min(100) as u8;
    let findings: Vec<String> = v
        .get("findings")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .take(5)
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let suggested_command = v
        .get("suggested_command")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty() && *s != "null")
        .map(str::to_owned);

    Some(QualityLlmReview {
        score,
        findings,
        suggested_command,
        llm_reviewed: true,
        input_tokens,
        output_tokens,
    })
}

/// Calls the adversary model and returns a [`QualityLlmReview`].
///
/// The `reviewer_model` string selects the provider family:
/// - `"claude-*"` → [`AnthropicProvider`] via `ANTHROPIC_API_KEY`
/// - anything else → [`OpenAiProvider`] via `OPENAI_API_KEY`
///
/// On any adapter or parse error this returns
/// [`QualityLlmReview::unavailable`] with `tier1_score`.
pub async fn review_turn(diff: &str, tier1_score: u8, reviewer_model: &str) -> QualityLlmReview {
    // Feed the full diff to the adversary reviewer rather than the former
    // 2 000-char cut, which hid most changes from review. A generous ceiling
    // still bounds token cost on pathologically large diffs.
    const MAX_DIFF_CHARS: usize = 100_000;
    let diff_bounded: String = diff.chars().take(MAX_DIFF_CHARS).collect();
    let prompt = RUBRIC
        .replace("{diff}", &diff_bounded)
        .replace("{tier1_score}", &tier1_score.to_string());

    let messages = vec![Message::user(prompt)];
    let opts = CallOptions {
        model: reviewer_model.to_owned(),
        max_tokens: Some(512),
        temperature: Some(0.1),
        system: None,
        tools: None,
        provider_session_id: None,
        smedja_session_id: None,
        permission_mode: None,
        stable_prefix_len: None,
        cache_strategy: smedja_adapter::CacheStrategy::None,
        workspace: None,
        tool_gate: None,
    };

    let (text, input_tokens, output_tokens) = if reviewer_model.starts_with("claude-") {
        call_anthropic(messages, opts).await
    } else {
        call_openai(messages, opts).await
    };

    if let Some(r) = parse_review_response(&text, input_tokens, output_tokens) {
        r
    } else {
        if !text.is_empty() {
            warn!(reviewer_model, "Tier-2 review response could not be parsed");
        }
        QualityLlmReview::unavailable(tier1_score)
    }
}

async fn call_anthropic(messages: Vec<Message>, opts: CallOptions) -> (String, u32, u32) {
    let Ok(key) = std::env::var("ANTHROPIC_API_KEY") else {
        warn!("ANTHROPIC_API_KEY not set; Tier-2 review unavailable");
        return (String::new(), 0, 0);
    };
    collect_stream(AnthropicProvider::new(key).stream_chat(&messages, &opts)).await
}

async fn call_openai(messages: Vec<Message>, opts: CallOptions) -> (String, u32, u32) {
    let Ok(key) = std::env::var("OPENAI_API_KEY") else {
        warn!("OPENAI_API_KEY not set; Tier-2 review unavailable");
        return (String::new(), 0, 0);
    };
    collect_stream(OpenAiProvider::new("https://api.openai.com", key).stream_chat(&messages, &opts))
        .await
}

async fn collect_stream(stream: smedja_adapter::provider::DeltaStream) -> (String, u32, u32) {
    let mut text = String::new();
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;
    let mut stream = stream;
    while let Some(item) = stream.next().await {
        match item {
            Ok(smedja_adapter::Delta::Text(t)) => text.push_str(&t),
            Ok(smedja_adapter::Delta::Usage {
                input_tokens: i,
                output_tokens: o,
                ..
            }) => {
                input_tokens = i;
                output_tokens = o;
            }
            Err(e) => {
                warn!(error = %e, "Tier-2 review stream error");
                break;
            }
            _ => {}
        }
    }
    (text, input_tokens, output_tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── quality_reviewer_model ─────────────────────────────────────────────

    #[test]
    fn openai_primary_routes_to_claude() {
        assert_eq!(
            quality_reviewer_model("openai"),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn openai_gpt4o_primary_routes_to_claude() {
        assert_eq!(
            quality_reviewer_model("openai/gpt-4o"),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn codex_primary_routes_to_claude() {
        assert_eq!(quality_reviewer_model("codex"), "claude-haiku-4-5-20251001");
    }

    #[test]
    fn claude_primary_routes_to_openai() {
        assert_eq!(quality_reviewer_model("claude"), "gpt-4o-mini");
    }

    #[test]
    fn haiku_primary_routes_to_openai() {
        assert_eq!(quality_reviewer_model("claude-haiku"), "gpt-4o-mini");
    }

    #[test]
    fn gemini_primary_routes_to_openai() {
        assert_eq!(quality_reviewer_model("gemini"), "gpt-4o-mini");
    }

    #[test]
    fn local_primary_routes_to_openai() {
        assert_eq!(quality_reviewer_model("local"), "gpt-4o-mini");
    }

    // ── QualityLlmReview::unavailable ─────────────────────────────────────

    #[test]
    fn unavailable_preserves_tier1_score() {
        let r = QualityLlmReview::unavailable(75);
        assert_eq!(r.score, 75);
        assert!(!r.llm_reviewed);
        assert!(!r.findings.is_empty());
        assert_eq!(r.input_tokens, 0);
        assert_eq!(r.output_tokens, 0);
    }

    // ── parse_review_response ─────────────────────────────────────────────

    #[test]
    fn parse_valid_json_response() {
        let text = r#"{"score": 75, "findings": ["missing test for fn foo"], "suggested_command": "/tdd-workflow"}"#;
        let r = parse_review_response(text, 100, 50).unwrap();
        assert_eq!(r.score, 75);
        assert_eq!(r.findings.len(), 1);
        assert!(r.findings[0].contains("missing test"));
        assert_eq!(r.suggested_command.as_deref(), Some("/tdd-workflow"));
        assert!(r.llm_reviewed);
        assert_eq!(r.input_tokens, 100);
        assert_eq!(r.output_tokens, 50);
    }

    #[test]
    fn parse_strips_code_fences() {
        let text = "```json\n{\"score\": 50, \"findings\": [], \"suggested_command\": null}\n```";
        let r = parse_review_response(text, 0, 0).unwrap();
        assert_eq!(r.score, 50);
        assert!(r.suggested_command.is_none());
    }

    #[test]
    fn parse_null_suggested_command_becomes_none() {
        let text = r#"{"score": 100, "findings": [], "suggested_command": "null"}"#;
        let r = parse_review_response(text, 0, 0).unwrap();
        assert!(r.suggested_command.is_none());
    }

    #[test]
    fn parse_caps_findings_at_five() {
        let text = r#"{"score": 50, "findings": ["a","b","c","d","e","f","g"], "suggested_command": null}"#;
        let r = parse_review_response(text, 0, 0).unwrap();
        assert_eq!(r.findings.len(), 5);
    }

    #[test]
    fn parse_malformed_json_returns_none() {
        assert!(parse_review_response("not json at all", 0, 0).is_none());
    }

    #[test]
    fn parse_missing_score_returns_none() {
        assert!(parse_review_response(r#"{"findings": []}"#, 0, 0).is_none());
    }
}
