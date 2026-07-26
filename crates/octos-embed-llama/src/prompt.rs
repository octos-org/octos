//! Prompt prefixes and post-pooling vector math.
//!
//! Kept outside the `embed-llama` feature gate: this is plain arithmetic and
//! string handling with no llama.cpp dependency, so a normal
//! `cargo test --workspace` exercises it everywhere.

/// EmbeddingGemma's task prefixes, from `config_sentence_transformers.json`.
/// They are prepended as PLAIN TEXT before tokenization, and the model was
/// trained with them — dropping them measurably degrades retrieval, so they are
/// not optional decoration.
///
/// Asymmetric by design: a query and the document it should match get different
/// prefixes.
pub const QUERY_PROMPT: &str = "task: search result | query: ";
pub const DOC_PROMPT: &str = "title: none | text: ";

/// Truncate an embedding to `out` dims and renormalize — Matryoshka (MRL).
///
/// EmbeddingGemma is trained so that a prefix of the vector is itself a valid
/// (weaker) embedding, which lets the index trade recall for memory. A no-op
/// when `out >= len`.
pub fn mrl_truncate(v: Vec<f32>, out: usize) -> Vec<f32> {
    if out >= v.len() {
        return v;
    }
    let head = &v[..out];
    let norm = head.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    head.iter().map(|x| x / norm).collect()
}

/// L2-normalize in place-ish, returning a unit vector.
///
/// llama.cpp's mean pooling does NOT normalize — `llama_get_embeddings_seq`
/// hands back the raw pooled vector. EmbeddingGemma's SentenceTransformers
/// pipeline ends in a `Normalize` module, so skipping this would leave cosine
/// similarity subtly wrong against a normalized index.
pub fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < f32::EPSILON {
        return;
    }
    for x in v.iter_mut() {
        *x /= norm;
    }
}

/// Apply the task prefix for the role being embedded.
pub fn with_prompt(text: &str, is_query: bool) -> String {
    if is_query {
        format!("{QUERY_PROMPT}{text}")
    } else {
        format!("{DOC_PROMPT}{text}")
    }
}

/// Group sequence indices into batches bounded by BOTH a sequence count and a
/// total-token budget.
///
/// The token budget is the load-bearing one: llama.cpp decodes a batch into a
/// single context, so the batch's combined token count must fit `n_ctx`. Sizing
/// the context for `max_batch` full-length sequences instead would allocate a KV
/// cache for tens of thousands of tokens that real inputs never use.
///
/// Indices are ordered by length first, so each batch packs texts of similar
/// size and a lone long document does not drag short ones into a big padded
/// batch. A sequence longer than `token_budget` still gets its own group rather
/// than being dropped — the caller truncates to the model limit beforehand.
pub fn batch_plan(lens: &[usize], max_batch: usize, token_budget: usize) -> Vec<Vec<usize>> {
    let max_batch = max_batch.max(1);
    let token_budget = token_budget.max(1);

    let mut order: Vec<usize> = (0..lens.len()).collect();
    order.sort_by_key(|&i| (lens[i], i));

    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut current_tokens = 0usize;

    for i in order {
        let would_exceed = current_tokens + lens[i] > token_budget;
        if !current.is_empty() && (current.len() >= max_batch || would_exceed) {
            groups.push(std::mem::take(&mut current));
            current_tokens = 0;
        }
        current_tokens += lens[i];
        current.push(i);
    }
    if !current.is_empty() {
        groups.push(current);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn prompts_are_asymmetric_and_prefix_the_text() {
        assert_eq!(with_prompt("hi", true), "task: search result | query: hi");
        assert_eq!(with_prompt("hi", false), "title: none | text: hi");
        assert_ne!(with_prompt("hi", true), with_prompt("hi", false));
    }

    #[test]
    fn l2_normalize_produces_a_unit_vector() {
        let mut v = vec![3.0, 4.0];
        l2_normalize(&mut v);
        assert!((norm(&v) - 1.0).abs() < 1e-6);
        assert!((v[0] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn l2_normalize_leaves_a_zero_vector_alone() {
        let mut v = vec![0.0, 0.0];
        l2_normalize(&mut v);
        assert!(v.iter().all(|x| x.is_finite()), "{v:?}");
    }

    #[test]
    fn mrl_truncate_shrinks_and_renormalizes() {
        let v = vec![0.5, 0.5, 0.5, 0.5];
        for d in [1usize, 2, 3] {
            let t = mrl_truncate(v.clone(), d);
            assert_eq!(t.len(), d);
            assert!((norm(&t) - 1.0).abs() < 1e-6, "dim {d}");
        }
        assert_eq!(mrl_truncate(v.clone(), 4), v, "no-op when not shrinking");
        assert_eq!(mrl_truncate(v.clone(), 9), v, "no-op when larger");
    }

    #[test]
    fn mrl_truncate_survives_a_zero_prefix() {
        let t = mrl_truncate(vec![0.0, 0.0, 1.0], 2);
        assert!(t.iter().all(|x| x.is_finite()), "{t:?}");
    }

    #[test]
    fn batch_plan_covers_every_index_once_and_respects_the_cap() {
        let lens = [7, 1, 9, 3, 3, 42];
        for max in [1usize, 2, 3, 16] {
            let plan = batch_plan(&lens, max, 100_000);
            assert!(plan.iter().all(|c| c.len() <= max && !c.is_empty()));
            let mut seen: Vec<usize> = plan.iter().flatten().copied().collect();
            seen.sort_unstable();
            assert_eq!(seen, (0..lens.len()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn batch_plan_keeps_a_long_outlier_away_from_short_texts() {
        let lens = [2, 1000, 3, 2];
        let plan = batch_plan(&lens, 2, 100_000);
        let group = plan.iter().find(|c| c.contains(&1)).unwrap();
        assert!(
            group.iter().filter(|&&i| i != 1).all(|&i| lens[i] >= 3),
            "short texts batched with the 1000-token outlier: {group:?}"
        );
    }

    /// The budget is what keeps a batch inside `n_ctx`; exceeding it is the bug
    /// that shows up as llama.cpp's opaque "invalid batch" (-1).
    #[test]
    fn batch_plan_never_exceeds_the_token_budget() {
        let lens = [100, 100, 100, 100, 100];
        let plan = batch_plan(&lens, 16, 250);
        for g in &plan {
            let total: usize = g.iter().map(|&i| lens[i]).sum();
            assert!(total <= 250, "group {g:?} totals {total} > 250");
        }
        let mut seen: Vec<usize> = plan.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..lens.len()).collect::<Vec<_>>());
    }

    /// A single sequence over budget must still be emitted, not dropped.
    #[test]
    fn batch_plan_keeps_an_oversized_sequence_in_its_own_group() {
        let lens = [10, 9999, 10];
        let plan = batch_plan(&lens, 16, 100);
        let group = plan.iter().find(|c| c.contains(&1)).expect("not dropped");
        assert_eq!(group, &vec![1], "oversized sequence gets a group to itself");
        let mut seen: Vec<usize> = plan.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[test]
    fn batch_plan_handles_empty_and_zero_cap() {
        assert!(batch_plan(&[], 4, 100).is_empty());
        assert_eq!(batch_plan(&[1, 2], 0, 100), vec![vec![0], vec![1]]);
    }
}
