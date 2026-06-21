//! Cosine similarity over `f32` slices.

/// Computes the cosine similarity between two equal-length `f32` vectors.
///
/// Returns a value in `[-1.0, 1.0]`. Returns `0.0` when either vector is the
/// zero vector (avoids division by zero).
///
/// The caller is responsible for ensuring `a` and `b` have the same length.
/// When the lengths differ the shorter slice drives the iterator and the
/// remaining components of the longer slice are ignored — this is intentional
/// (the outer query loop validates dimensions before calling).
pub(crate) fn cosine_sim(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let mag_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let mag_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        0.0
    } else {
        dot / (mag_a * mag_b)
    }
}

#[cfg(test)]
mod tests {
    use super::cosine_sim;

    #[test]
    fn cosine_sim_identical_vectors() {
        let score = cosine_sim(&[1.0_f32, 0.0], &[1.0, 0.0]);
        assert!(
            (score - 1.0_f32).abs() < 1e-6,
            "identical unit vectors must score 1.0, got {score}"
        );
    }

    #[test]
    fn cosine_sim_orthogonal_vectors() {
        let score = cosine_sim(&[1.0_f32, 0.0], &[0.0, 1.0]);
        assert!(
            score.abs() < 1e-6,
            "orthogonal vectors must score 0.0, got {score}"
        );
    }

    #[test]
    fn cosine_sim_zero_vector() {
        let score = cosine_sim(&[0.0_f32, 0.0], &[1.0, 1.0]);
        assert!(
            score.abs() < 1e-6,
            "zero vector must score 0.0 (no div-by-zero), got {score}"
        );
    }
}
