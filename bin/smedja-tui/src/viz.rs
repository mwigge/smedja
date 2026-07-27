//! Tiny visualization helpers shared by the rail panels (sparklines).

/// Eighth-block ramp, low → high, for inline trend sparklines.
const SPARK_RAMP: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Renders `values` as an inline block sparkline against a fixed `max`, one
/// glyph per value. Empty input yields an empty string; a zero `max` floors to
/// 1 so the ramp never divides by zero. Values above `max` clamp to the top
/// block. The fixed scale (vs. per-window min/max) keeps a trend comparable
/// across polls — a run of 100s always reads full-height.
#[must_use]
pub fn sparkline(values: &[u8], max: u8) -> String {
    if values.is_empty() {
        return String::new();
    }
    let max = f64::from(max.max(1));
    values
        .iter()
        .map(|&v| {
            #[allow(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss
            )]
            let idx = ((f64::from(v) / max) * 7.0).round() as usize;
            SPARK_RAMP[idx.min(7)]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparkline_scales_max_to_top_block() {
        assert_eq!(sparkline(&[100], 100), "█");
        assert_eq!(sparkline(&[0], 100), "▁");
    }

    #[test]
    fn sparkline_empty_input_is_empty_string() {
        assert!(sparkline(&[], 100).is_empty());
    }
}
