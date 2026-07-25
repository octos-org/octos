//! Pure helpers for the embedder: Matryoshka truncation and batch planning.
//!
//! Deliberately kept OUTSIDE the `embed-mlx` platform/feature gate. Everything
//! here is plain arithmetic with no MLX dependency, so it is exercised by a
//! normal `cargo test --workspace` on every platform — unlike the parity and
//! benchmark suites, which need Apple Silicon and the cached model and are
//! therefore `#[ignore]`d.

#![allow(dead_code)] // consumed only by the feature-gated provider

/// Truncate an embedding to `out` dims and renormalize (matches the Python MRL
/// path: `l2(full[:, :d])`). A no-op when `out >= len`.
pub(crate) fn mrl_truncate(v: Vec<f32>, out: usize) -> Vec<f32> {
    if out >= v.len() {
        return v;
    }
    let head = &v[..out];
    let norm = head.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    head.iter().map(|x| x / norm).collect()
}

/// Group sequence indices into forward-pass batches of at most `max_batch`.
///
/// Indices are ordered by token length so each batch pads to a length close to
/// its own members' — batching a 5-token query with a 2000-token document would
/// otherwise cost 2000 columns of wasted attention for both. The caller
/// scatters results back into input order via the returned indices.
pub(crate) fn batch_plan(lens: &[usize], max_batch: usize) -> Vec<Vec<usize>> {
    let mut order: Vec<usize> = (0..lens.len()).collect();
    // Tie-break on index so the plan is deterministic for equal lengths.
    order.sort_by_key(|&i| (lens[i], i));
    order
        .chunks(max_batch.max(1))
        .map(<[usize]>::to_vec)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn mrl_truncate_is_noop_when_not_shrinking() {
        let v = vec![0.6, 0.8];
        assert_eq!(mrl_truncate(v.clone(), 2), v);
        assert_eq!(mrl_truncate(v.clone(), 9), v);
    }

    #[test]
    fn mrl_truncate_shrinks_and_renormalizes_to_unit_norm() {
        // 4-d unit vector; every prefix must come back unit-norm.
        let v = vec![0.5, 0.5, 0.5, 0.5];
        for d in [1usize, 2, 3] {
            let t = mrl_truncate(v.clone(), d);
            assert_eq!(t.len(), d);
            assert!((norm(&t) - 1.0).abs() < 1e-6, "dim {d}: norm {}", norm(&t));
        }
    }

    #[test]
    fn mrl_truncate_preserves_direction() {
        let v = vec![3.0, 4.0, 100.0];
        let t = mrl_truncate(v, 2);
        // 3:4 ratio survives; magnitude is renormalized to 1.
        assert!((t[0] - 0.6).abs() < 1e-6);
        assert!((t[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn mrl_truncate_does_not_divide_by_zero_on_zero_prefix() {
        let t = mrl_truncate(vec![0.0, 0.0, 1.0], 2);
        assert!(t.iter().all(|x| x.is_finite()), "{t:?}");
    }

    #[test]
    fn batch_plan_covers_every_index_exactly_once() {
        let lens = [7, 1, 9, 3, 3, 42];
        let plan = batch_plan(&lens, 2);
        let mut seen: Vec<usize> = plan.iter().flatten().copied().collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..lens.len()).collect::<Vec<_>>());
    }

    #[test]
    fn batch_plan_respects_max_batch() {
        let lens = [1, 2, 3, 4, 5, 6, 7];
        for max in [1usize, 2, 3, 16] {
            let plan = batch_plan(&lens, max);
            assert!(plan.iter().all(|c| c.len() <= max && !c.is_empty()));
        }
    }

    #[test]
    fn batch_plan_groups_similar_lengths() {
        // The long outlier must not share a batch with the short sequences.
        let lens = [2, 1000, 3, 2];
        let plan = batch_plan(&lens, 2);
        let with_outlier = plan.iter().find(|c| c.contains(&1)).unwrap();
        let others: Vec<usize> = with_outlier.iter().copied().filter(|&i| i != 1).collect();
        assert!(
            others.iter().all(|&i| lens[i] >= 3),
            "short sequences batched with the 1000-token outlier: {with_outlier:?}"
        );
    }

    #[test]
    fn batch_plan_is_deterministic_for_equal_lengths() {
        let lens = [5, 5, 5, 5];
        assert_eq!(batch_plan(&lens, 2), batch_plan(&lens, 2));
        assert_eq!(batch_plan(&lens, 2), vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn batch_plan_handles_empty_and_zero_max() {
        assert!(batch_plan(&[], 4).is_empty());
        // max_batch 0 must not panic or loop forever — it clamps to 1.
        assert_eq!(batch_plan(&[1, 2], 0), vec![vec![0], vec![1]]);
    }
}
