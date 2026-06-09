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
    if score >= 0.50 {
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

/// Query-independent maintenance relevance (ARCHITECTURE-PLAN §9.3 `R_base`). The
/// `graph_centrality` term is omitted in M6 (centrality lands with associations, M7).
pub fn score_base(score: f64, access_count: i64, source_trust: f64, state: &str) -> f64 {
    let access_freq = ((access_count.max(0) as f64).ln_1p() / ACCESS_SAT.ln_1p()).clamp(0.0, 1.0);
    let base =
        0.40 * score + 0.20 * access_freq + 0.13 * source_trust + 0.15 * lifecycle_bonus_pos(state);
    base.clamp(0.0, 1.0)
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
    let mut decayed = 0_usize;
    let mut tokens = 0_i64;
    let mut budget_hit = false;
    let mut partial = false;

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
        // A sub-full batch means we drained the backlog: stop without a spurious extra
        // iteration (which could trip the wall-clock and falsely mark the run partial).
        if batch.raw_consumed < CONSOLIDATE_BATCH {
            break;
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
    store.finish_dream_run(&run_id, clock(), touched, touched, tokens, status)?;

    Ok(DreamOutcome {
        run_id,
        consolidated,
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
        let lo = score_base(0.1, 0, 0.5, "decaying");
        let hi = score_base(0.9, 0, 0.5, "active");
        assert!(hi > lo);
        assert!((0.0..=1.0).contains(&score_base(1.0, 100, 1.0, "active")));
    }

    #[test]
    fn normalize_collapses_whitespace() {
        assert_eq!(normalize("  a\tb\n c  "), "a b c");
    }
}
