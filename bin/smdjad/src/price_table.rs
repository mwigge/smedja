//! Bundled model pricing table loaded from `prices.toml` at compile time.

use std::collections::HashMap;

use serde::Deserialize;

const BUNDLED: &str = include_str!("../prices.toml");

#[derive(Debug, Deserialize)]
struct RawPrices {
    input: HashMap<String, f64>,
    output: HashMap<String, f64>,
}

/// Per-model pricing in USD per 1 M tokens.
#[derive(Debug)]
pub struct PriceTable {
    input: HashMap<String, f64>,
    output: HashMap<String, f64>,
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
        let cost =
            (f64::from(input_tok) * in_price + f64::from(output_tok) * out_price) / 1_000_000.0;
        cost
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
}
