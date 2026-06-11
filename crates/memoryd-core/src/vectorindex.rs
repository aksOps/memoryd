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
        // total_cmp keeps the order total even if a NaN score ever appears
        // (matching the HNSW implementation's comparator).
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| b.id.cmp(&a.id)));
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

/// Select a `VectorIndex` by config kind. Unknown kinds fall back to the safe default
/// (`BruteForce`); `Config::validate` rejects unknown kinds up front so this is only a
/// defensive backstop.
pub fn from_kind(kind: &str) -> Box<dyn VectorIndex> {
    match kind {
        "hnsw" => Box::new(Hnsw::default()),
        _ => Box::new(BruteForce),
    }
}

/// In-process HNSW (Hierarchical Navigable Small World) index — a dependency-free,
/// deterministic, `unsafe`-free second `VectorIndex` implementation (ARCHITECTURE-PLAN
/// §21.12). Works in cosine-similarity space ("closer" = higher cosine).
///
/// The `VectorIndex` trait is stateless (candidates are handed in per call), so the
/// graph is built per `search` call. Over the small FTS-prefiltered shortlist the
/// current recall pipeline passes (≤ `RECALL_CANDIDATE_CAP`), that build cost makes
/// HNSW *slower* than `BruteForce` — so `BruteForce` stays the default and the
/// correctness oracle. HNSW's latency win needs a persistent full-corpus index, which
/// is deferred; this milestone lands the algorithm, the config seam, and recall@10
/// parity with the oracle.
#[derive(Debug, Clone, Copy)]
pub struct Hnsw {
    /// Max neighbors per node per upper layer (`2*m` at layer 0).
    m: usize,
    /// Dynamic candidate list size during graph construction.
    ef_construction: usize,
    /// Dynamic candidate list size during query.
    ef_search: usize,
}

impl Default for Hnsw {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 64,
            ef_search: 64,
        }
    }
}

impl VectorIndex for Hnsw {
    fn search(&self, query: &[f32], candidates: &[Candidate], k: usize) -> Vec<Scored> {
        if candidates.is_empty() || k == 0 {
            return Vec::new();
        }
        // Small shortlists: the graph build is not worth it and degenerate graphs hurt
        // recall — score exactly (identical to BruteForce).
        if candidates.len() <= self.m {
            return BruteForce.search(query, candidates, k);
        }
        let graph = HnswGraph::build(candidates, self.m, self.ef_construction);
        let found = graph.query(query, self.ef_search.max(k));
        // Rank the found set exactly as BruteForce does (sim desc, id DESC) so the only
        // possible difference from the oracle is *which* candidates were found, not order.
        let mut scored: Vec<Scored> = found
            .into_iter()
            .map(|nb| Scored {
                id: candidates[nb.node].id,
                score: nb.sim,
            })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| b.id.cmp(&a.id)));
        scored.truncate(k);
        scored
    }
}

/// A (node, similarity) pair with a total order: similarity, then node index. Lets a
/// `BinaryHeap` act as a max-heap on similarity with deterministic tie-breaks.
#[derive(Debug, Clone, Copy)]
struct Neighbor {
    sim: f32,
    node: usize,
}

impl PartialEq for Neighbor {
    fn eq(&self, other: &Self) -> bool {
        self.node == other.node && self.sim.to_bits() == other.sim.to_bits()
    }
}
impl Eq for Neighbor {}
impl Ord for Neighbor {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.sim
            .total_cmp(&other.sim)
            .then(self.node.cmp(&other.node))
    }
}
impl PartialOrd for Neighbor {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// A built HNSW graph over a candidate slice (borrowed for the lifetime of one search).
struct HnswGraph<'a> {
    vectors: Vec<&'a [f32]>,
    /// `links[node][layer]` = neighbor node indices at that layer.
    links: Vec<Vec<Vec<usize>>>,
    entry: usize,
    max_layer: usize,
    m: usize,
    ef_construction: usize,
}

impl<'a> HnswGraph<'a> {
    fn build(candidates: &'a [Candidate], m: usize, ef_construction: usize) -> Self {
        let n = candidates.len();
        let vectors: Vec<&[f32]> = candidates.iter().map(|c| c.vector.as_slice()).collect();
        let m_l = 1.0 / (m as f64).ln();
        let levels: Vec<usize> = (0..n).map(|i| level_for(i, m_l)).collect();

        let mut graph = HnswGraph {
            vectors,
            links: (0..n).map(|i| vec![Vec::new(); levels[i] + 1]).collect(),
            entry: 0,
            max_layer: levels[0],
            m,
            ef_construction,
        };

        for (node, &level) in levels.iter().enumerate().skip(1) {
            graph.insert(node, level);
        }
        graph
    }

    fn cap(&self, layer: usize) -> usize {
        if layer == 0 { self.m * 2 } else { self.m }
    }

    fn insert(&mut self, node: usize, level: usize) {
        // Phase 1: greedy-descend from the entry point through layers above `level`.
        let mut ep = self.entry;
        let mut lc = self.max_layer;
        while lc > level {
            let found = self.search_layer(self.vectors[node], &[ep], 1, lc);
            if let Some(best) = found.first() {
                ep = best.node;
            }
            lc -= 1;
        }

        // Phase 2: connect at each layer from min(level, max_layer) down to 0.
        let top = level.min(self.max_layer);
        let mut entry_points = vec![ep];
        for layer in (0..=top).rev() {
            let found = self.search_layer(
                self.vectors[node],
                &entry_points,
                self.ef_construction,
                layer,
            );
            let cap = self.cap(layer);
            let selected = select_neighbors(&self.vectors, node, &found, cap);
            for &nbr in &selected {
                self.links[node][layer].push(nbr);
                self.links[nbr][layer].push(node);
                if self.links[nbr][layer].len() > self.cap(layer) {
                    let pruned = select_neighbors_ids(
                        &self.vectors,
                        nbr,
                        &self.links[nbr][layer].clone(),
                        self.cap(layer),
                    );
                    self.links[nbr][layer] = pruned;
                }
            }
            entry_points = found.iter().map(|nb| nb.node).collect();
            if entry_points.is_empty() {
                entry_points = vec![ep];
            }
        }

        if level > self.max_layer {
            self.max_layer = level;
            self.entry = node;
        }
    }

    /// Greedy best-first search within one layer; returns up to `ef` neighbors sorted
    /// by descending similarity. Deterministic (total-ordered heaps, id tie-breaks).
    fn search_layer(
        &self,
        query: &[f32],
        entry_points: &[usize],
        ef: usize,
        layer: usize,
    ) -> Vec<Neighbor> {
        use std::cmp::Reverse;
        use std::collections::{BinaryHeap, HashSet};

        let mut visited: HashSet<usize> = HashSet::new();
        let mut candidates: BinaryHeap<Neighbor> = BinaryHeap::new();
        let mut result: BinaryHeap<Reverse<Neighbor>> = BinaryHeap::new();

        for &ep in entry_points {
            if visited.insert(ep) {
                let sim = cosine(query, self.vectors[ep]);
                let nb = Neighbor { sim, node: ep };
                candidates.push(nb);
                result.push(Reverse(nb));
            }
        }

        while let Some(current) = candidates.pop() {
            let worst = result.peek().map(|r| r.0.sim);
            if let Some(worst) = worst
                && result.len() >= ef
                && current.sim < worst
            {
                break;
            }
            if layer >= self.links[current.node].len() {
                continue;
            }
            for &nbr in &self.links[current.node][layer] {
                if visited.insert(nbr) {
                    let sim = cosine(query, self.vectors[nbr]);
                    let worst = result.peek().map(|r| r.0.sim);
                    if result.len() < ef || worst.is_none_or(|w| sim > w) {
                        let nb = Neighbor { sim, node: nbr };
                        candidates.push(nb);
                        result.push(Reverse(nb));
                        if result.len() > ef {
                            result.pop();
                        }
                    }
                }
            }
        }

        let mut out: Vec<Neighbor> = result.into_iter().map(|r| r.0).collect();
        out.sort_by(|a, b| b.cmp(a));
        out
    }

    fn query(&self, query: &[f32], ef: usize) -> Vec<Neighbor> {
        let mut ep = self.entry;
        let mut lc = self.max_layer;
        while lc >= 1 {
            let found = self.search_layer(query, &[ep], 1, lc);
            if let Some(best) = found.first() {
                ep = best.node;
            }
            lc -= 1;
        }
        self.search_layer(query, &[ep], ef, 0)
    }
}

/// FNV-1a (64-bit) over the little-endian bytes of each input word. Unlike
/// `DefaultHasher` — whose algorithm the std docs reserve the right to change between
/// Rust releases — FNV-1a has a fixed, specified output, so the HNSW level assignment
/// is reproducible across toolchain versions, not merely within one binary. No `rand`
/// dependency.
fn fnv1a(words: &[u64]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for &word in words {
        for byte in word.to_le_bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// Deterministic geometric level for a node, hash-seeded (no `rand` dependency):
/// `floor(-ln(u) * m_l)` with `u` a stable uniform in `(0, 1)` derived from the index.
fn level_for(index: usize, m_l: f64) -> usize {
    let raw = fnv1a(&[index as u64, 0x9E37_79B9_7F4A_7C15]);
    // 53-bit mantissa mapped to (0, 1): never 0 (so -ln is finite), never 1.
    let u = ((raw >> 11) as f64 + 1.0) / (((1u64 << 53) as f64) + 1.0);
    (-u.ln() * m_l).floor() as usize
}

/// Pick the `m` candidates (from `found`) closest to `target` — simple heuristic, by
/// cosine then node index (deterministic). Excludes `target` itself.
fn select_neighbors(vectors: &[&[f32]], target: usize, found: &[Neighbor], m: usize) -> Vec<usize> {
    let ids: Vec<usize> = found.iter().map(|nb| nb.node).collect();
    select_neighbors_ids(vectors, target, &ids, m)
}

fn select_neighbors_ids(vectors: &[&[f32]], target: usize, ids: &[usize], m: usize) -> Vec<usize> {
    let mut scored: Vec<Neighbor> = ids
        .iter()
        .filter(|&&id| id != target)
        .map(|&id| Neighbor {
            sim: cosine(vectors[target], vectors[id]),
            node: id,
        })
        .collect();
    scored.sort_by(|a, b| b.cmp(a));
    scored.truncate(m);
    scored.into_iter().map(|nb| nb.node).collect()
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

    #[test]
    fn brute_force_equal_scores_tie_break_by_id_desc() {
        let query = vec![1.0, 0.0];
        let candidates = vec![
            Candidate {
                id: 7,
                vector: vec![1.0, 0.0],
            },
            Candidate {
                id: 9,
                vector: vec![1.0, 0.0],
            },
            Candidate {
                id: 8,
                vector: vec![1.0, 0.0],
            },
        ];
        let ranked = BruteForce.search(&query, &candidates, 3);
        let ids: Vec<i64> = ranked.iter().map(|s| s.id).collect();
        assert_eq!(ids, [9, 8, 7], "identical scores order by id DESC");
    }

    #[test]
    fn brute_force_sort_is_total_with_nan_scores() {
        // NaN components produce NaN cosine scores; total_cmp must keep the
        // sort total (no panic, deterministic placement) and rank real
        // matches ahead of NaN (total_cmp orders NaN above +1.0, so NaN
        // candidates sort first — the assertion pins that behavior).
        let query = vec![1.0, 0.0];
        let candidates = vec![
            Candidate {
                id: 1,
                vector: vec![f32::NAN, 0.0],
            },
            Candidate {
                id: 2,
                vector: vec![1.0, 0.0],
            },
        ];
        let ranked = BruteForce.search(&query, &candidates, 2);
        assert_eq!(ranked.len(), 2, "sort completes with NaN present");
        assert!(
            ranked.iter().any(|s| s.id == 2),
            "real match is not dropped"
        );
    }

    #[test]
    fn hnsw_sort_is_total_with_nan_scores() {
        // Mirror of brute_force_sort_is_total_with_nan_scores for the HNSW
        // path (shortlist > M so the graph path's own sort runs): total_cmp
        // keeps the sort total (no panic, deterministic) and a NaN candidate
        // never drops the real top hit.
        let mut candidates = fixture(100, 8);
        candidates.push(Candidate {
            id: 9999,
            vector: vec![f32::NAN; 8],
        });
        let query = candidates[10].vector.clone();
        let ranked = Hnsw::default().search(&query, &candidates, 10);
        assert_eq!(ranked.len(), 10, "sort completes with NaN present");
        assert!(
            ranked.iter().any(|s| s.id == 10),
            "real match is not dropped"
        );
    }

    /// Deterministic fixture vectors (no `rand`): each component is a stable FNV-1a
    /// hash of (id, dim) mapped into [-1, 1].
    fn fixture(n: usize, dim: usize) -> Vec<Candidate> {
        (0..n)
            .map(|i| {
                let vector = (0..dim)
                    .map(|j| {
                        let raw = fnv1a(&[i as u64, j as u64, 0xA5A5_5A5A]);
                        (((raw >> 11) as f64 / ((1u64 << 53) as f64)) * 2.0 - 1.0) as f32
                    })
                    .collect();
                Candidate {
                    id: i as i64,
                    vector,
                }
            })
            .collect()
    }

    fn ids(scored: &[Scored]) -> Vec<i64> {
        scored.iter().map(|s| s.id).collect()
    }

    #[test]
    fn hnsw_matches_brute_force_top1_exact() {
        let candidates = fixture(300, 16);
        // Query = a fixture vector with a tiny deterministic perturbation -> #42 is clearly nearest.
        let mut query = candidates[42].vector.clone();
        query[0] += 0.01;
        let bf = BruteForce.search(&query, &candidates, 10);
        let hnsw = Hnsw::default().search(&query, &candidates, 10);
        assert_eq!(hnsw[0].id, bf[0].id, "HNSW top-1 matches the oracle");
        assert_eq!(bf[0].id, 42, "the perturbed vector's own id is nearest");
    }

    #[test]
    fn hnsw_recall_at_10_within_epsilon() {
        let candidates = fixture(400, 24);
        let query = candidates[123].vector.clone();
        let bf = ids(&BruteForce.search(&query, &candidates, 10));
        let hnsw = ids(&Hnsw::default().search(&query, &candidates, 10));
        let bf_set: std::collections::HashSet<i64> = bf.iter().copied().collect();
        let overlap = hnsw.iter().filter(|id| bf_set.contains(id)).count();
        assert!(
            overlap >= 9,
            "recall@10 within epsilon: overlap {overlap}/10\n  bf={bf:?}\n  hnsw={hnsw:?}"
        );
        assert_eq!(hnsw.len(), 10, "returns exactly k results");
    }

    #[test]
    fn hnsw_small_n_is_exact_like_brute_force() {
        // N <= M -> exact (BruteForce fallback), identical ordering.
        let candidates = fixture(8, 4);
        let query = candidates[3].vector.clone();
        let bf = BruteForce.search(&query, &candidates, 5);
        let hnsw = Hnsw::default().search(&query, &candidates, 5);
        assert_eq!(ids(&hnsw), ids(&bf));
    }

    #[test]
    fn hnsw_empty_and_k_zero() {
        let candidates = fixture(50, 8);
        assert!(
            Hnsw::default()
                .search(&candidates[0].vector, &[], 5)
                .is_empty()
        );
        assert!(
            Hnsw::default()
                .search(&candidates[0].vector, &candidates, 0)
                .is_empty()
        );
    }

    #[test]
    fn hnsw_is_deterministic() {
        let candidates = fixture(200, 12);
        let query = candidates[7].vector.clone();
        let a = Hnsw::default().search(&query, &candidates, 10);
        let b = Hnsw::default().search(&query, &candidates, 10);
        assert_eq!(a, b, "same input -> identical output");
    }

    #[test]
    fn hnsw_handles_mismatched_dims_without_panic() {
        let mut candidates = fixture(100, 16);
        // Inject a wrong-dim candidate: cosine() returns 0, so it must not outrank real hits.
        candidates.push(Candidate {
            id: 9999,
            vector: vec![1.0, 2.0, 3.0],
        });
        let query = candidates[10].vector.clone();
        let hnsw = Hnsw::default().search(&query, &candidates, 10);
        assert!(
            !hnsw.iter().any(|s| s.id == 9999),
            "zero-similarity dim-mismatch not in top-10"
        );
    }

    #[test]
    fn from_kind_selects_implementation() {
        // Both implement the trait; on a fixture they agree on the top hit.
        let candidates = fixture(120, 16);
        let query = candidates[5].vector.clone();
        let brute = from_kind("brute-force").search(&query, &candidates, 5);
        let hnsw = from_kind("hnsw").search(&query, &candidates, 5);
        let unknown = from_kind("nonsense").search(&query, &candidates, 5);
        assert_eq!(brute[0].id, hnsw[0].id);
        assert_eq!(
            unknown[0].id, brute[0].id,
            "unknown kind falls back to brute-force"
        );
    }
}
