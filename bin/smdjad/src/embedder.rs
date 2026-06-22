//! Lightweight bag-of-words hash embedding for vault search.
//!
//! Produces a fixed-dimension vector from plain text using FNV-1a hashing.
//! Suitable for keyword-similarity search without an external embedding model.
//!
//! All vectors are L2-normalised so cosine similarity equals the dot product.

/// Dimension of the embedding vector produced by [`embed`].
pub const DIM: usize = 128;

/// Embeds `text` into a [`DIM`]-dimensional L2-normalised vector.
///
/// Words are lowercased and hashed into buckets; each bucket accumulates the
/// count of words that hash to it. The resulting count vector is then L2-
/// normalised. Two texts that share many words will have a high cosine score.
///
/// Returns a zero vector for empty input.
pub fn embed(text: &str) -> Vec<f32> {
    let mut vec = vec![0.0_f32; DIM];
    for word in text.split_whitespace() {
        let bucket = fnv1a(word.to_lowercase().as_bytes()) % DIM;
        vec[bucket] += 1.0;
    }
    l2_normalize(&mut vec);
    vec
}

fn fnv1a(bytes: &[u8]) -> usize {
    let mut hash: usize = 14_695_981_039_346_656_037_usize;
    for &b in bytes {
        hash ^= b as usize;
        hash = hash.wrapping_mul(1_099_511_628_211);
    }
    hash
}

fn l2_normalize(vec: &mut [f32]) {
    let norm = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-9 {
        for x in vec.iter_mut() {
            *x /= norm;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_empty_text_returns_zero_vector() {
        let v = embed("");
        assert_eq!(v.len(), DIM);
        assert!(
            v.iter().all(|&x| x == 0.0),
            "empty text must produce zero vector"
        );
    }

    #[test]
    fn embed_produces_dim_length_vector() {
        let v = embed("hello world");
        assert_eq!(v.len(), DIM);
    }

    #[test]
    fn embed_is_normalised() {
        let v = embed("rust programming language");
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-5,
            "embedding must be L2-normalised; norm={norm}"
        );
    }

    #[test]
    fn identical_text_produces_identical_vector() {
        let a = embed("consistent hashing");
        let b = embed("consistent hashing");
        assert_eq!(a, b);
    }

    #[test]
    fn overlapping_text_scores_higher_than_disjoint() {
        let base = embed("rust async await tokio");
        let similar = embed("rust tokio networking");
        let different = embed("python django orm migrations");

        let sim_similar: f32 = base.iter().zip(similar.iter()).map(|(a, b)| a * b).sum();
        let sim_different: f32 = base.iter().zip(different.iter()).map(|(a, b)| a * b).sum();

        assert!(
            sim_similar > sim_different,
            "overlapping text must score higher than disjoint text; \
             similar={sim_similar:.4}, different={sim_different:.4}"
        );
    }
}
