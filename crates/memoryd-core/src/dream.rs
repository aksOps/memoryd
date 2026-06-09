//! Dream plane: scheduled/explicit consolidation of `raw_events` into durable
//! `memories`, plus lifecycle decay — under a wall-clock + provider-spend cap, with
//! `dream_runs` accounting. This module holds the pure scoring/decay helpers and the
//! `dream_once` orchestrator; the transactional DB work lives on [`crate::store::Store`].
//!
//! M6 scope: lexical dedup-cluster consolidation (LLM summarization is exercised only
//! by a metered test-double; the shipped `null` adapter degrades to a deterministic
//! lexical representative) and decay lifecycle transitions over due rows. Association
//! centrality (M7), profile/approvals (M8), and §9.7 cleanup/purge tiers are deferred.

use crate::adapters::ProviderAdapter;
use crate::config::Caps;
use crate::store::{Store, StoreError};

const DAY_MS: i64 = 86_400_000;
/// `decay_at` lands `DORMANT_HALVINGS` half-lives out (~6.25% of original strength).
pub const DORMANT_HALVINGS: i64 = 4;
/// Grace after a memory goes dormant before it is archived (ARCHITECTURE-PLAN §9.6).
pub const ARCHIVE_GRACE_MS: i64 = 30 * DAY_MS;
/// `access_frequency` saturation (§9.1).
const ACCESS_SAT: f64 = 50.0;
/// `Δt` (in half-lives) at which `decay_score` crosses the dormant threshold (0.15):
/// `log2(1/0.15) ≈ 2.737`.
const DORMANT_HALFLIFE_FACTOR: f64 = 2.737;

const CONSOLIDATE_BATCH: usize = 1000;
const DECAY_BATCH: usize = 500;

// M7 association graph (ARCHITECTURE-PLAN §9.3/§9.4, §21.10).
/// Degree-strength saturation for `centrality = clamp(Σ weight / SAT, 0, 1)` (§9.4).
pub const CENTRALITY_SAT: f64 = 8.0;
/// Per-node outbound link cap; growth is bounded by construction (§21.10).
pub const ASSOCIATE_FANOUT_CAP: usize = 32;
/// Links at or below this weight are pruned (count stays bounded over runs).
pub const WEAK_LINK_FLOOR: f64 = 0.10;
/// Starting weight for a freshly observed co-occurrence edge.
pub const CO_OCCUR_BASE: f64 = 0.30;
/// Reinforcement increment applied each time a co-occurrence is re-observed.
pub const CO_OCCUR_REINFORCE: f64 = 0.10;
/// Minimum cosine for an embedding-similarity (`semantic`) edge; below it, no link.
pub const SEM_LINK_THRESHOLD: f64 = 0.20;
/// Cap on co-occurrence group size before pairing (bounds the O(n²) pairing per run).
pub const CO_OCCUR_GROUP_CAP: usize = 64;
pub(crate) const ASSOCIATE_BATCH: usize = 500;
/// Recall recency half-life: a hit accessed this long ago scores 0.5 on the recency term.
const RECENCY_HALFLIFE_MS: i64 = 7 * DAY_MS;
/// Recall-time fusion weights (ARCHITECTURE-PLAN §9.3 `[scoring.recall]`). These sum to
/// 0.95 by design: `lifecycle_bonus`/`provenance`/`supersession` are *additive,
/// unweighted* terms (per §9.3), which is why `score_recall` clamps to [0, 1.45] rather
/// than [0, 1].
const RW_SEM: f64 = 0.34;
const RW_LEX: f64 = 0.18;
const RW_REC: f64 = 0.12;
const RW_ACC: f64 = 0.08;
const RW_LINK: f64 = 0.10;
const RW_CENT: f64 = 0.06;
const RW_TRUST: f64 = 0.07;

/// Per-`kind` decay half-life in ms; `None` = never decays (identity/profile, H6).
pub fn half_life_ms(kind: &str) -> Option<i64> {
    let days = match kind {
        "identity" => return None,
        "preference" => 180,
        "fact" => 120,
        "decision" => 90,
        "task" | "todo" => 21,
        "ephemeral" => 3,
        // "observation" / ambient and anything unmapped
        _ => 14,
    };
    Some(days * DAY_MS)
}

/// Map a `raw_events.kind` onto a `memories.kind` that has a decay half-life.
pub fn memory_kind_for(raw_kind: &str) -> &'static str {
    match raw_kind {
        "preference" => "preference",
        "fact" => "fact",
        "decision" => "decision",
        "task" | "todo" => "task",
        "ephemeral" => "ephemeral",
        _ => "observation",
    }
}

/// Fixed `source_trust` per source/kind (ARCHITECTURE-PLAN §9.5, minimal M6 subset).
pub fn trust_for_source(source: &str, kind: &str) -> f64 {
    if kind == "import" || source == "import" {
        0.50
    } else if source == "tool_result" {
        0.75
    } else if source == "cli" {
        0.70
    } else {
        0.55
    }
}

/// `decay_factor = exp(-ln2 · Δt / half_life)`; clamps `Δt < 0` (clock skew) to 1.0.
pub fn decay_score(dt_ms: i64, half_life: i64) -> f64 {
    if dt_ms <= 0 || half_life <= 0 {
        return 1.0;
    }
    (-std::f64::consts::LN_2 * dt_ms as f64 / half_life as f64).exp()
}

/// Canonical lifecycle state for a given decay score / age. State is a pure function
/// of `Δt`, so access (which resets `last_accessed_at`) naturally revives a memory.
pub fn lifecycle_for(
    score: f64,
    dt_ms: i64,
    half_life: i64,
    archive_grace_ms: i64,
) -> &'static str {
    if score > 0.50 {
        "active"
    } else if score >= 0.15 {
        "decaying"
    } else if dt_ms
        >= half_life
            .saturating_mul(DORMANT_HALVINGS)
            .saturating_add(archive_grace_ms)
    {
        "archived"
    } else {
        "dormant"
    }
}

/// The `decay_at` checkpoint when the memory's state would next change — keeps the
/// decay sweep scan-free and self-terminating (the row re-surfaces only at its next
/// transition, never in a tight loop).
pub fn next_decay_at(
    state: &str,
    last_accessed: i64,
    half_life: i64,
    archive_grace_ms: i64,
) -> Option<i64> {
    match state {
        "active" => Some(last_accessed + half_life),
        "decaying" => Some(last_accessed + (half_life as f64 * DORMANT_HALFLIFE_FACTOR) as i64),
        "dormant" => Some(last_accessed + half_life * DORMANT_HALVINGS + archive_grace_ms),
        _ => None,
    }
}

/// Positive lifecycle bonus (clamped to [0,1]) used in the maintenance base score.
fn lifecycle_bonus_pos(state: &str) -> f64 {
    match state {
        "active" => 0.20,
        "associated" => 0.15,
        "consolidated" => 0.10,
        _ => 0.0,
    }
}

/// Signed lifecycle bonus in [-0.30, +0.20] used in recall ranking (§9.3): favors live
/// memories, penalizes decaying/dormant/archived ones.
pub fn lifecycle_bonus_signed(state: &str) -> f64 {
    match state {
        "active" => 0.20,
        "associated" => 0.15,
        "consolidated" => 0.10,
        "decaying" => -0.10,
        "dormant" => -0.20,
        "archived" => -0.30,
        _ => 0.0,
    }
}

/// Log-saturating access frequency in [0,1] (§9.1), shared by maintenance and recall.
pub fn access_frequency(access_count: i64) -> f64 {
    ((access_count.max(0) as f64).ln_1p() / ACCESS_SAT.ln_1p()).clamp(0.0, 1.0)
}

/// Query-independent maintenance relevance (ARCHITECTURE-PLAN §9.3 `R_base`). M7 wires
/// in the `graph_centrality` term (weight 0.12, completing the §9.3 base weights to 1.0).
pub fn score_base(
    score: f64,
    access_count: i64,
    source_trust: f64,
    state: &str,
    centrality: f64,
) -> f64 {
    let access_freq = access_frequency(access_count);
    let base = 0.40 * score
        + 0.20 * access_freq
        + 0.12 * centrality.clamp(0.0, 1.0)
        + 0.13 * source_trust
        + 0.15 * lifecycle_bonus_pos(state);
    base.clamp(0.0, 1.0)
}

/// Degree-weighted centrality (§9.4): `clamp(Σ incident link weight / CENTRALITY_SAT, 0, 1)`.
/// Bounded and local — each memory needs only its own incident `memory_links` rows.
pub fn centrality_for(incident_weight_sum: f64) -> f64 {
    (incident_weight_sum / CENTRALITY_SAT).clamp(0.0, 1.0)
}

/// Recency term for recall: `exp(-ln2 · age / RECENCY_HALFLIFE_MS)`, clamped to [0,1].
pub fn recency_term(age_ms: i64) -> f64 {
    if age_ms <= 0 {
        return 1.0;
    }
    (-std::f64::consts::LN_2 * age_ms as f64 / RECENCY_HALFLIFE_MS as f64).exp()
}

/// Inputs to the recall-time relevance fusion (ARCHITECTURE-PLAN §9.3 `R_recall`).
#[derive(Debug, Clone, Copy)]
pub struct RecallTerms {
    /// Cosine-derived semantic similarity in [0,1] (0 when no usable embedding).
    pub semantic: f64,
    /// Lexical (bm25) match, min-max normalized within the candidate set, in [0,1].
    pub lexical: f64,
    /// Recency in [0,1] (see [`recency_term`]).
    pub recency: f64,
    /// Access frequency in [0,1] (log-saturating).
    pub access_freq: f64,
    /// Link strength of the edge a hop-neighbor was reached by; 0 for direct hits.
    pub link_strength: f64,
    /// Graph centrality, min-max normalized within the candidate set, in [0,1].
    pub centrality: f64,
    /// Source trust in [0,1].
    pub source_trust: f64,
    /// Signed lifecycle bonus in [-0.30, +0.20].
    pub lifecycle_bonus: f64,
}

/// Recall-time relevance `R_recall` (§9.3). `semantic_available=false` triggers the
/// documented degrade rule: the freed `w_sem` mass is folded into `w_lex` (recall stays
/// lexical-but-ranked rather than collapsing to FTS order). Result clamped to [0, 1.45].
pub fn score_recall(t: &RecallTerms, semantic_available: bool) -> f64 {
    let (w_sem, w_lex) = if semantic_available {
        (RW_SEM, RW_LEX)
    } else {
        (0.0, RW_LEX + RW_SEM)
    };
    let r = w_sem * t.semantic
        + w_lex * t.lexical
        + RW_REC * t.recency
        + RW_ACC * t.access_freq
        + RW_LINK * t.link_strength
        + RW_CENT * t.centrality
        + RW_TRUST * t.source_trust
        + t.lifecycle_bonus;
    r.clamp(0.0, 1.45)
}

/// Normalize text for dedup-clustering: trim + collapse internal whitespace.
pub fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// How a dream run was triggered and how tightly it is capped.
pub struct DreamOptions {
    pub trigger: &'static str,
    pub budget_usd: f64,
    pub max_seconds: u64,
}

/// Result of one dream run, mirrored into the `dream_runs` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DreamOutcome {
    pub run_id: String,
    pub consolidated: usize,
    pub associated: usize,
    pub decayed: usize,
    pub tokens_used: i64,
    pub status: &'static str,
}

/// Run one dream pass: consolidate pending raw_events, then decay due memories, under
/// a wall-clock budget and a provider-spend cap. `clock` is the single time source
/// (production: `unix_ms_now`; tests inject a controllable clock to trip the cap
/// deterministically). Returns the `dream_runs` accounting.
pub fn dream_once<A: ProviderAdapter>(
    store: &mut Store,
    adapter: &A,
    _caps: &Caps,
    opts: &DreamOptions,
    clock: &dyn Fn() -> i64,
) -> Result<DreamOutcome, StoreError> {
    let start = clock();
    let run_id = store.create_dream_run(opts.trigger, start)?;
    let max_ms = i64::try_from(opts.max_seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1000);

    let mut window_spend = 0.0_f64;
    let mut consolidated = 0_usize;
    let mut associated = 0_usize;
    let mut decayed = 0_usize;
    let mut tokens = 0_i64;
    let mut budget_hit = false;
    let mut partial = false;
    let mut jobs_run = 0_i64;

    // Consolidate phase: drain pending raw_events in bounded batches.
    loop {
        if clock() - start >= max_ms {
            partial = true;
            break;
        }
        let batch = store.consolidate_pending(
            adapter,
            opts.budget_usd,
            &mut window_spend,
            &run_id,
            CONSOLIDATE_BATCH,
            clock(),
        )?;
        consolidated += batch.memories_created;
        tokens += batch.tokens;
        budget_hit |= batch.budget_hit;
        if batch.raw_consumed > 0 {
            jobs_run += 1;
        }
        // A sub-full batch means we drained the backlog: stop without a spurious extra
        // iteration (which could trip the wall-clock and falsely mark the run partial).
        if batch.raw_consumed < CONSOLIDATE_BATCH {
            break;
        }
    }

    // Associate phase: build/reinforce/prune the graph over recall-eligible memories
    // (one bounded batch per run). Runs before decay so decay's R_base sees fresh
    // centrality. Skipped if the wall-clock already tripped during consolidation.
    if !partial && clock() - start < max_ms {
        let assoc = store.associate_pending(adapter, start, clock())?;
        associated += assoc.nodes_associated;
        if assoc.links_created
            + assoc.links_reinforced
            + assoc.links_pruned
            + assoc.nodes_associated
            > 0
        {
            jobs_run += 1;
        }
    }

    // Decay phase: a fixed `now` for the whole phase so a row recomputed this run is
    // not re-selected (its decay_at advances past `now`).
    if !partial {
        let decay_now = clock();
        loop {
            if clock() - start >= max_ms {
                partial = true;
                break;
            }
            let batch = store.decay_due(DECAY_BATCH, decay_now)?;
            decayed += batch.touched;
            if batch.touched == 0 {
                break;
            }
            jobs_run += 1;
        }
    }

    let status = if partial {
        "partial"
    } else if budget_hit {
        "budget_capped"
    } else {
        "completed"
    };
    let touched = i64::try_from(consolidated + decayed).unwrap_or(i64::MAX);
    store.finish_dream_run(&run_id, clock(), jobs_run, touched, tokens, status)?;

    Ok(DreamOutcome {
        run_id,
        consolidated,
        associated,
        decayed,
        tokens_used: tokens,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decay_score_halves_at_one_half_life() {
        let hl = 14 * DAY_MS;
        assert!((decay_score(hl, hl) - 0.5).abs() < 1e-9);
        assert_eq!(decay_score(0, hl), 1.0);
        assert_eq!(decay_score(-5, hl), 1.0);
    }

    #[test]
    fn lifecycle_thresholds_follow_canonical_order() {
        let hl = 14 * DAY_MS;
        // fresh -> active; ~1.5 half-lives -> decaying; ~3 -> dormant; very old -> archived
        assert_eq!(
            lifecycle_for(decay_score(hl / 2, hl), hl / 2, hl, ARCHIVE_GRACE_MS),
            "active"
        );
        assert_eq!(
            lifecycle_for(
                decay_score(hl * 3 / 2, hl),
                hl * 3 / 2,
                hl,
                ARCHIVE_GRACE_MS
            ),
            "decaying"
        );
        assert_eq!(
            lifecycle_for(decay_score(hl * 3, hl), hl * 3, hl, ARCHIVE_GRACE_MS),
            "dormant"
        );
        let very_old = hl * DORMANT_HALVINGS + ARCHIVE_GRACE_MS + 1;
        assert_eq!(
            lifecycle_for(decay_score(very_old, hl), very_old, hl, ARCHIVE_GRACE_MS),
            "archived"
        );
    }

    #[test]
    fn next_decay_at_advances_per_state() {
        let hl = 14 * DAY_MS;
        assert_eq!(next_decay_at("active", 0, hl, ARCHIVE_GRACE_MS), Some(hl));
        assert!(next_decay_at("decaying", 0, hl, ARCHIVE_GRACE_MS).unwrap() > hl);
        assert_eq!(next_decay_at("archived", 0, hl, ARCHIVE_GRACE_MS), None);
    }

    #[test]
    fn identity_kind_never_decays() {
        assert_eq!(half_life_ms("identity"), None);
        assert_eq!(half_life_ms("fact"), Some(120 * DAY_MS));
        assert_eq!(half_life_ms("whatever"), Some(14 * DAY_MS));
    }

    #[test]
    fn score_base_is_bounded_and_monotonic_in_decay() {
        let lo = score_base(0.1, 0, 0.5, "decaying", 0.0);
        let hi = score_base(0.9, 0, 0.5, "active", 0.0);
        assert!(hi > lo);
        assert!((0.0..=1.0).contains(&score_base(1.0, 100, 1.0, "active", 1.0)));
    }

    #[test]
    fn score_base_centrality_term_raises_score() {
        let without = score_base(0.5, 0, 0.5, "active", 0.0);
        let with = score_base(0.5, 0, 0.5, "active", 1.0);
        // centrality weight is 0.12; a fully-central node scores 0.12 higher (pre-clamp).
        assert!(
            (with - without - 0.12).abs() < 1e-9,
            "with={with} without={without}"
        );
    }

    #[test]
    fn centrality_for_saturates_at_cap() {
        assert_eq!(centrality_for(0.0), 0.0);
        assert!((centrality_for(4.0) - 0.5).abs() < 1e-9);
        assert_eq!(centrality_for(CENTRALITY_SAT), 1.0);
        assert_eq!(centrality_for(100.0), 1.0);
    }

    #[test]
    fn score_recall_degrade_folds_sem_into_lex() {
        let terms = RecallTerms {
            semantic: 0.0,
            lexical: 1.0,
            recency: 0.0,
            access_freq: 0.0,
            link_strength: 0.0,
            centrality: 0.0,
            source_trust: 0.0,
            lifecycle_bonus: 0.0,
        };
        // Degrade: w_lex' = w_lex + w_sem = 0.18 + 0.34 = 0.52 applied to lexical=1.0.
        let degraded = score_recall(&terms, false);
        assert!((degraded - 0.52).abs() < 1e-9, "got {degraded}");
        // With semantic available but sem=0, only w_lex=0.18 applies.
        let normal = score_recall(&terms, true);
        assert!((normal - 0.18).abs() < 1e-9, "got {normal}");
    }

    #[test]
    fn score_recall_rewards_link_and_centrality() {
        let base = RecallTerms {
            semantic: 0.0,
            lexical: 0.0,
            recency: 0.0,
            access_freq: 0.0,
            link_strength: 0.0,
            centrality: 0.0,
            source_trust: 0.0,
            lifecycle_bonus: 0.0,
        };
        let linked = RecallTerms {
            link_strength: 1.0,
            centrality: 1.0,
            ..base
        };
        assert!(score_recall(&linked, true) > score_recall(&base, true));
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  a\tb\n c  "), "a b c");
    }
}
