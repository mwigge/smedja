//! `smj models` — provider model catalog (list / show).

use anyhow::Result;
use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum ModelsCmd {
    /// List all models in the catalog, optionally filtered by provider
    List {
        /// Show only models from this provider (e.g. openai, anthropic, ollama)
        #[arg(long)]
        provider: Option<String>,
    },
    /// Show catalog entry for a specific model id
    Show {
        /// Model id to inspect (e.g. gpt-4o)
        model_id: String,
    },
}

/// A model entry in the catalog.
#[derive(Debug, Clone)]
struct ModelInfo {
    /// Provider name (e.g. "openai", "anthropic").
    provider: &'static str,
    /// Model identifier as used in API calls.
    id: &'static str,
    /// Context window in tokens, when known.
    context_window: Option<usize>,
    /// Input cost in USD per million tokens, when known.
    input_usd_mtk: Option<f64>,
    /// Output cost in USD per million tokens, when known.
    output_usd_mtk: Option<f64>,
}

/// Static model catalog — expanded from provider docs.
///
/// ponytail: static slice is sufficient; add dynamic registry when providers expose it.
static MODEL_CATALOG: &[ModelInfo] = &[
    ModelInfo {
        provider: "openai",
        id: "gpt-4o",
        context_window: Some(128_000),
        input_usd_mtk: Some(2.50),
        output_usd_mtk: Some(10.00),
    },
    ModelInfo {
        provider: "openai",
        id: "gpt-4o-mini",
        context_window: Some(128_000),
        input_usd_mtk: Some(0.15),
        output_usd_mtk: Some(0.60),
    },
    ModelInfo {
        provider: "openai",
        id: "o1",
        context_window: Some(200_000),
        input_usd_mtk: Some(15.00),
        output_usd_mtk: Some(60.00),
    },
    ModelInfo {
        provider: "anthropic",
        id: "claude-opus-4-5",
        context_window: Some(200_000),
        input_usd_mtk: Some(15.00),
        output_usd_mtk: Some(75.00),
    },
    ModelInfo {
        provider: "anthropic",
        id: "claude-sonnet-4-5",
        context_window: Some(200_000),
        input_usd_mtk: Some(3.00),
        output_usd_mtk: Some(15.00),
    },
    ModelInfo {
        provider: "anthropic",
        id: "claude-haiku-3-5",
        context_window: Some(200_000),
        input_usd_mtk: Some(0.80),
        output_usd_mtk: Some(4.00),
    },
    ModelInfo {
        provider: "groq",
        id: "llama-3.3-70b-versatile",
        context_window: Some(128_000),
        input_usd_mtk: Some(0.59),
        output_usd_mtk: Some(0.79),
    },
    ModelInfo {
        provider: "deepseek",
        id: "deepseek-chat",
        context_window: Some(64_000),
        input_usd_mtk: Some(0.14),
        output_usd_mtk: Some(0.28),
    },
    ModelInfo {
        provider: "together",
        id: "meta-llama/Llama-3-8b-chat-hf",
        context_window: Some(8_192),
        input_usd_mtk: Some(0.20),
        output_usd_mtk: Some(0.20),
    },
    ModelInfo {
        provider: "perplexity",
        id: "sonar-pro",
        context_window: Some(127_072),
        input_usd_mtk: Some(3.00),
        output_usd_mtk: Some(15.00),
    },
    ModelInfo {
        provider: "xai",
        id: "grok-3-beta",
        context_window: Some(131_072),
        input_usd_mtk: Some(3.00),
        output_usd_mtk: Some(15.00),
    },
    ModelInfo {
        provider: "ollama",
        id: "llama3.2",
        context_window: Some(128_000),
        input_usd_mtk: None,
        output_usd_mtk: None,
    },
    ModelInfo {
        provider: "bedrock",
        id: "anthropic.claude-3-5-sonnet-20241022-v2:0",
        context_window: Some(200_000),
        input_usd_mtk: Some(3.00),
        output_usd_mtk: Some(15.00),
    },
    ModelInfo {
        provider: "bedrock",
        id: "anthropic.claude-3-haiku-20240307-v1:0",
        context_window: Some(200_000),
        input_usd_mtk: Some(0.25),
        output_usd_mtk: Some(1.25),
    },
    ModelInfo {
        provider: "bedrock",
        id: "amazon.nova-pro-v1:0",
        context_window: Some(300_000),
        input_usd_mtk: Some(0.80),
        output_usd_mtk: Some(3.20),
    },
    ModelInfo {
        provider: "bedrock",
        id: "amazon.nova-lite-v1:0",
        context_window: Some(300_000),
        input_usd_mtk: Some(0.06),
        output_usd_mtk: Some(0.24),
    },
];

/// Formats a `ModelInfo` slice as table lines (header + rows).
fn format_models_table(rows: &[&ModelInfo]) -> Vec<String> {
    let mut out = Vec::with_capacity(rows.len() + 2);
    out.push(format!(
        "{:<12} {:<36} {:>10}  {:>12}  {:>13}",
        "PROVIDER", "MODEL", "CONTEXT", "INPUT_$/MTK", "OUTPUT_$/MTK"
    ));
    out.push("-".repeat(90));
    for m in rows {
        let ctx = m
            .context_window
            .map_or_else(|| "-".to_owned(), |n| n.to_string());
        let inp = m
            .input_usd_mtk
            .map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"));
        let out_cost = m
            .output_usd_mtk
            .map_or_else(|| "-".to_owned(), |v| format!("{v:.2}"));
        out.push(format!(
            "{:<12} {:<36} {:>10}  {:>12}  {:>13}",
            m.provider, m.id, ctx, inp, out_cost
        ));
    }
    out
}

/// Dispatches a `smj models` subcommand.
// Result signature kept uniform with the other subcommand `run` fns for `?` dispatch.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn run(action: ModelsCmd) -> Result<()> {
    match action {
        ModelsCmd::List { provider } => {
            let rows: Vec<&ModelInfo> = MODEL_CATALOG
                .iter()
                .filter(|m| {
                    provider
                        .as_deref()
                        .is_none_or(|p| m.provider.eq_ignore_ascii_case(p))
                })
                .collect();
            for line in format_models_table(&rows) {
                println!("{line}");
            }
        }
        ModelsCmd::Show { model_id } => {
            let rows: Vec<&ModelInfo> = MODEL_CATALOG
                .iter()
                .filter(|m| m.id.eq_ignore_ascii_case(&model_id))
                .collect();
            if rows.is_empty() {
                eprintln!("no model found with id '{model_id}'");
                std::process::exit(1);
            }
            for line in format_models_table(&rows) {
                println!("{line}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_list_formats_table_header() {
        let rows: Vec<&ModelInfo> = MODEL_CATALOG.iter().collect();
        let lines = format_models_table(&rows);
        let header = &lines[0];
        assert!(
            header.contains("MODEL"),
            "header must contain MODEL: {header}"
        );
        assert!(
            header.contains("PROVIDER"),
            "header must contain PROVIDER: {header}"
        );
        assert!(
            header.contains("CONTEXT"),
            "header must contain CONTEXT: {header}"
        );
    }

    #[test]
    fn models_show_filters_to_id() {
        let target = "gpt-4o";
        let rows: Vec<&ModelInfo> = MODEL_CATALOG
            .iter()
            .filter(|m| m.id.eq_ignore_ascii_case(target))
            .collect();
        assert!(!rows.is_empty(), "gpt-4o must be in the catalog");
        let lines = format_models_table(&rows);
        // Should have header + separator + exactly one data row.
        assert_eq!(
            lines.len(),
            3,
            "show should return header + separator + one row"
        );
        assert!(
            lines[2].contains("gpt-4o"),
            "data row must contain model id"
        );
        assert!(
            lines[2].contains("openai"),
            "data row must name the provider"
        );
    }

    #[test]
    fn models_list_includes_bedrock_entries() {
        let bedrock_rows: Vec<&ModelInfo> = MODEL_CATALOG
            .iter()
            .filter(|m| m.provider == "bedrock")
            .collect();
        assert!(
            !bedrock_rows.is_empty(),
            "bedrock must have at least one entry in MODEL_CATALOG"
        );
        let ids: Vec<&str> = bedrock_rows.iter().map(|m| m.id).collect();
        assert!(
            ids.contains(&"anthropic.claude-3-5-sonnet-20241022-v2:0"),
            "Bedrock Sonnet 3.5 v2 must be in the catalog"
        );
        assert!(
            ids.contains(&"amazon.nova-pro-v1:0"),
            "Bedrock Nova Pro must be in the catalog"
        );
        // All bedrock entries must have a known context window and pricing.
        for entry in &bedrock_rows {
            assert!(
                entry.context_window.is_some(),
                "bedrock entry {} must have a context_window",
                entry.id
            );
            assert!(
                entry.input_usd_mtk.is_some(),
                "bedrock entry {} must have input pricing",
                entry.id
            );
            assert!(
                entry.output_usd_mtk.is_some(),
                "bedrock entry {} must have output pricing",
                entry.id
            );
        }
    }
}
