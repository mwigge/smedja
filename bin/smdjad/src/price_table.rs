//! Bundled model pricing table loaded from `prices.toml` at compile time.

use std::collections::HashMap;

use serde::Deserialize;

const BUNDLED: &str = include_str!("../prices.toml");

/// Default context window in tokens used when a model is not in `[windows]`.
const DEFAULT_WINDOW: u32 = 200_000;

#[derive(Debug, Deserialize)]
struct RawPrices {
    input: HashMap<String, f64>,
    output: HashMap<String, f64>,
    windows: HashMap<String, u32>,
}

/// Per-model pricing and context window sizes.
#[derive(Debug)]
pub struct PriceTable {
    input: HashMap<String, f64>,
    output: HashMap<String, f64>,
    windows: HashMap<String, u32>,
}

impl PriceTable {
    /// Parses the `prices.toml` that was embedded at compile time.
    ///
    /// # Panics
    ///
    /// Panics if the embedded TOML is malformed — this is a build-time invariant.
    #[must_use]
    pub fn embedded() -> Self {
        let raw: RawPrices =
            toml::from_str(BUNDLED).expect("prices.toml embedded at compile time must be valid");
        Self {
            input: raw.input,
            output: raw.output,
            windows: raw.windows,
        }
    }

    /// Returns the USD cost for `input_tok` input tokens and `output_tok` output
    /// tokens using the price entry for `model`.
    ///
    /// Returns `0.0` when the model is not in the price table — usage is still
    /// recorded, the cost just shows as zero rather than failing the turn.
    #[must_use]
    pub fn compute_cost(&self, model: &str, input_tok: u32, output_tok: u32) -> f64 {
        let in_price = self.input.get(model).copied().unwrap_or(0.0);
        let out_price = self.output.get(model).copied().unwrap_or(0.0);
        (f64::from(input_tok) * in_price + f64::from(output_tok) * out_price) / 1_000_000.0
    }

    /// Returns the context window size in tokens for `model`.
    ///
    /// Falls back to [`DEFAULT_WINDOW`] (200 K) when the model is not in the
    /// table — safe to display as a best-effort estimate.
    #[must_use]
    pub fn context_window(&self, model: &str) -> u32 {
        self.windows.get(model).copied().unwrap_or(DEFAULT_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_parses_without_panic() {
        let _ = PriceTable::embedded();
    }

    #[test]
    fn known_model_returns_nonzero_cost() {
        let pt = PriceTable::embedded();
        let cost = pt.compute_cost("claude-sonnet-4-6", 1_000_000, 0);
        assert!(
            (cost - 3.0).abs() < 1e-9,
            "1M input tokens at $3/M should be $3.00; got {cost}"
        );
    }

    #[test]
    fn output_tokens_priced_separately() {
        let pt = PriceTable::embedded();
        let cost = pt.compute_cost("claude-sonnet-4-6", 0, 1_000_000);
        assert!(
            (cost - 15.0).abs() < 1e-9,
            "1M output tokens at $15/M should be $15.00; got {cost}"
        );
    }

    #[test]
    fn unknown_model_returns_zero() {
        let pt = PriceTable::embedded();
        let cost = pt.compute_cost("model-not-in-table", 100_000, 50_000);
        assert!(
            cost.abs() < f64::EPSILON,
            "unknown model should yield $0.0; got {cost}"
        );
    }

    #[test]
    fn zero_tokens_returns_zero() {
        let pt = PriceTable::embedded();
        let cost = pt.compute_cost("claude-sonnet-4-6", 0, 0);
        assert!(cost.abs() < f64::EPSILON);
    }

    #[test]
    fn context_window_known_claude_model() {
        let pt = PriceTable::embedded();
        assert_eq!(
            pt.context_window("claude-sonnet-4-6"),
            200_000,
            "Claude Sonnet should have 200K window"
        );
    }

    #[test]
    fn context_window_known_openai_model() {
        let pt = PriceTable::embedded();
        assert_eq!(
            pt.context_window("gpt-4o-mini"),
            128_000,
            "gpt-4o-mini should have 128K window"
        );
    }

    #[test]
    fn context_window_unknown_model_defaults_to_200k() {
        let pt = PriceTable::embedded();
        assert_eq!(
            pt.context_window("unknown-model"),
            DEFAULT_WINDOW,
            "unknown model should fall back to DEFAULT_WINDOW"
        );
    }
}
