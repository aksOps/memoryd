//! Vector index seam. M4 ships the brute-force implementation — cosine over a
//! bounded candidate shortlist (never the whole table, H7). M9 swaps in an
//! in-process HNSW behind this same trait with no change to recall callers.

/// A candidate vector to be ranked against a query.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub id: i64,
    pub vector: Vec<f32>,
}

/// A scored candidate (cosine similarity in `[-1, 1]`).
#[derive(Debug, Clone, PartialEq)]
pub struct Scored {
    pub id: i64,
    pub score: f32,
}

/// Rank candidate vectors by similarity to a query vector.
pub trait VectorIndex {
    /// Return the top-`k` candidates by descending similarity. Implementations
    /// must only compare the candidates handed in — never a wider corpus.
    fn search(&self, query: &[f32], candidates: &[Candidate], k: usize) -> Vec<Scored>;
}

/// Brute-force cosine over the provided shortlist.
#[derive(Debug, Clone, Copy, Default)]
pub struct BruteForce;

impl VectorIndex for BruteForce {
    fn search(&self, query: &[f32], candidates: &[Candidate], k: usize) -> Vec<Scored> {
        let mut scored: Vec<Scored> = candidates
            .iter()
            .map(|candidate| Scored {
                id: candidate.id,
                score: cosine(query, &candidate.vector),
            })
            .collect();
        // Descending similarity; ties broken by id DESC for determinism.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.id.cmp(&a.id))
        });
        scored.truncate(k);
        scored
    }
}

/// Cosine similarity. Returns 0.0 when either vector is zero-length, has a zero
/// norm, or the dimensions differ (no partial comparison across models).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical_is_one_orthogonal_is_zero() {
        assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_handles_zero_and_mismatched_dims() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
        assert_eq!(cosine(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn brute_force_ranks_by_similarity_and_truncates_to_k() {
        let query = vec![1.0, 0.0, 0.0];
        let candidates = vec![
            Candidate {
                id: 1,
                vector: vec![0.0, 1.0, 0.0],
            }, // orthogonal -> 0
            Candidate {
                id: 2,
                vector: vec![1.0, 0.0, 0.0],
            }, // identical -> 1
            Candidate {
                id: 3,
                vector: vec![0.9, 0.1, 0.0],
            }, // close -> high
        ];
        let ranked = BruteForce.search(&query, &candidates, 2);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].id, 2);
        assert_eq!(ranked[1].id, 3);
    }

    #[test]
    fn brute_force_empty_candidates_is_empty() {
        assert!(BruteForce.search(&[1.0], &[], 5).is_empty());
    }
}
