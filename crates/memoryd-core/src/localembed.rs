//! In-process semantic embeddings: bge-small-en-v1.5 (fp32 ONNX, 384-dim) executed by
//! tract — pure Rust, no network, no GPU, no C runtime. The model bytes are embedded
//! into the binary at build time (`build.rs` pins their SHA-256), so the daemon is
//! self-contained and air-gap safe; quality/latency evidence lives in the
//! ARCHITECTURE-PLAN local-adapter callout.
//!
//! Inference plans are compiled per input-length bucket (a tract plan has a fixed
//! input shape) and cached for the process lifetime: short texts run on small, fast
//! plans; anything longer than the largest bucket is truncated. Plan compilation
//! costs seconds, so it happens lazily — long-lived processes (`serve`) amortize it,
//! one-shot CLI calls pay it once on first semantic use.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokenizers::Tokenizer;
use tract_onnx::prelude::*;

/// Output dimension of bge-small-en-v1.5.
pub const EMBED_DIM: usize = 384;
/// Stable model identifier recorded in `embeddings.model_id` / `provider_usage`.
pub const MODEL_ID: &str = "bge-small-en-v1.5";
/// bge-v1.5 retrieval recipe: queries carry this instruction prefix, passages don't.
const QUERY_PREFIX: &str = "Represent this sentence for searching relevant passages: ";
/// Token-length buckets; each gets its own compiled plan. 256 is the hard cap
/// (longer inputs truncate — mean memory snippets are far shorter).
const BUCKETS: [usize; 5] = [16, 32, 64, 128, 256];

static MODEL_BYTES: &[u8] = include_bytes!(env!("MEMORYD_BGE_MODEL"));
static TOKENIZER_BYTES: &[u8] = include_bytes!(env!("MEMORYD_BGE_TOKENIZER"));

static TOKENIZER: OnceLock<Result<Tokenizer, String>> = OnceLock::new();
type PlanCache = Mutex<HashMap<usize, Arc<TypedSimplePlan>>>;
static PLANS: OnceLock<PlanCache> = OnceLock::new();

/// Embed a stored passage (memory/raw-event content).
pub fn embed_passage(text: &str) -> Result<Vec<f32>, String> {
    embed_raw(text)
}

/// Embed a recall query (bge asymmetric-retrieval prefix applied).
pub fn embed_query(text: &str) -> Result<Vec<f32>, String> {
    embed_raw(&format!("{QUERY_PREFIX}{text}"))
}

fn embed_raw(text: &str) -> Result<Vec<f32>, String> {
    let tokenizer = tokenizer()?;
    let encoding = tokenizer
        .encode(text, true)
        .map_err(|e| format!("tokenize failed: {e}"))?;
    let ids = encoding.get_ids();
    let mask = encoding.get_attention_mask();
    let types = encoding.get_type_ids();

    let max = BUCKETS[BUCKETS.len() - 1];
    let take = ids.len().min(max);
    let bucket = BUCKETS.iter().copied().find(|&b| b >= take).unwrap_or(max);

    let plan = plan_for(bucket)?;
    let fill = |src: &[u32]| -> Tensor {
        let mut v = vec![0i64; bucket];
        for (dst, &x) in v.iter_mut().zip(src.iter().take(take)) {
            *dst = i64::from(x);
        }
        tract_ndarray::Array2::from_shape_vec((1, bucket), v)
            .expect("shape (1, bucket) matches vec length")
            .into()
    };
    let outputs = plan
        .run(tvec!(
            fill(ids).into(),
            fill(mask).into(),
            fill(types).into()
        ))
        .map_err(|e| format!("inference failed: {e}"))?;

    // CLS pooling (token 0 of last_hidden_state), then L2 normalize — the bge recipe.
    let hidden = outputs[0]
        .to_plain_array_view::<f32>()
        .map_err(|e| format!("output read failed: {e}"))?;
    if hidden.ndim() != 3 || hidden.shape()[2] != EMBED_DIM {
        return Err(format!(
            "unexpected output shape {:?} (want [1, seq, {EMBED_DIM}])",
            hidden.shape()
        ));
    }
    let mut vector: Vec<f32> = (0..EMBED_DIM).map(|i| hidden[[0, 0, i]]).collect();
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut vector {
            *x /= norm;
        }
    }
    Ok(vector)
}

fn tokenizer() -> Result<&'static Tokenizer, String> {
    TOKENIZER
        .get_or_init(|| {
            Tokenizer::from_bytes(TOKENIZER_BYTES)
                .map_err(|e| format!("tokenizer load failed: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

fn plan_for(bucket: usize) -> Result<Arc<TypedSimplePlan>, String> {
    let cache = PLANS.get_or_init(|| Mutex::new(HashMap::new()));
    // The lock is deliberately held across build_plan: on the small target VM,
    // letting concurrent threads compile the same multi-second plan in parallel
    // (double-checked locking) costs more CPU and peak memory than briefly
    // serializing cold-start. The cache is bounded (5 fixed buckets), so the
    // contention window only exists until the working buckets are warm.
    let mut guard = cache
        .lock()
        .map_err(|_| "plan cache poisoned".to_string())?;
    if let Some(plan) = guard.get(&bucket) {
        return Ok(plan.clone());
    }
    let plan = build_plan(bucket)?;
    guard.insert(bucket, plan.clone());
    Ok(plan)
}

fn build_plan(bucket: usize) -> Result<Arc<TypedSimplePlan>, String> {
    let mut model = onnx()
        .model_for_read(&mut &MODEL_BYTES[..])
        .map_err(|e| format!("model parse failed: {e}"))?;
    let inputs = model
        .input_outlets()
        .map_err(|e| format!("model inputs unavailable: {e}"))?
        .len();
    for i in 0..inputs {
        model
            .set_input_fact(i, i64::fact([1, bucket]).into())
            .map_err(|e| format!("input fact failed: {e}"))?;
    }
    model
        .into_optimized()
        .and_then(|optimized| optimized.into_runnable())
        .map_err(|e| format!("plan build failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cosine(a: &[f32], b: &[f32]) -> f32 {
        a.iter().zip(b).map(|(x, y)| x * y).sum()
    }

    #[test]
    fn embeds_are_deterministic_normalized_384d() {
        let a = embed_passage("I prefer dark mode in all my editors").expect("embed ok");
        let b = embed_passage("I prefer dark mode in all my editors").expect("embed ok");
        assert_eq!(a, b, "same input, identical vector");
        assert_eq!(a.len(), EMBED_DIM);
        let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "L2-normalized, got {norm}");
    }

    #[test]
    fn semantic_neighbors_rank_above_unrelated_text() {
        let anchor = embed_passage("I prefer dark mode in all my editors").expect("embed ok");
        let similar = embed_passage("the user likes dark themes for coding tools").expect("ok");
        let unrelated = embed_passage("the quarterly finance report is due next week").expect("ok");
        assert!(
            cosine(&anchor, &similar) > cosine(&anchor, &unrelated) + 0.1,
            "semantic signal: sim {:.3} vs diff {:.3}",
            cosine(&anchor, &similar),
            cosine(&anchor, &unrelated)
        );
    }

    #[test]
    fn query_prefix_changes_the_vector_but_keeps_the_neighborhood() {
        let passage = embed_passage("dark mode preference").expect("ok");
        let query = embed_query("dark mode preference").expect("ok");
        assert_ne!(passage, query, "query instruction prefix must be applied");
        assert!(
            cosine(&passage, &query) > 0.5,
            "still the same neighborhood"
        );
    }

    #[test]
    fn long_input_truncates_instead_of_failing() {
        let long = "memory ".repeat(2000);
        let v = embed_passage(&long).expect("truncated embed ok");
        assert_eq!(v.len(), EMBED_DIM);
    }

    #[test]
    fn empty_input_embeds_without_panic() {
        let v = embed_passage("").expect("empty embed ok");
        assert_eq!(v.len(), EMBED_DIM);
    }
}
