//! Near-duplicate detection against the vector index.
//!
//! Detection only: this module answers "what is already in the index that
//! looks like this?" and nothing else. Turning ids into a caller-facing
//! report — loading summaries, truncating them — belongs to the transport
//! adapters, which is what keeps this function synchronous and therefore
//! unable to `await` while the index read lock is held.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::model::{DecayPhase, MemoryId, NamespaceId};
use crate::search::{FlatVectorIndex, SearchFilter, VectorIndex, is_normalized};

/// Tolerance on `| ||v||^2 - 1 |` for calling a query vector normalized.
const NORM_EPSILON: f32 = 1e-3;

/// Set the first time a non-normalized query vector is seen, so the
/// `warn!` fires once per process instead of once per store.
static UNNORMALIZED_WARNED: AtomicBool = AtomicBool::new(false);

/// Find existing memories at least `threshold` similar to `embedding`,
/// closest first, at most `limit` of them.
///
/// Deliberately **not** [`QueryEngine::similar`](crate::search::QueryEngine):
/// that requires the memory to already exist, and it records an associative
/// access on every result it returns. At store time that would age every
/// neighbour's FSRS schedule on every single write — a decay-model change
/// smuggled in behind an advisory string.
///
/// Never errors. A dimension mismatch, an empty index, or a query vector
/// that is not L2-normalized all yield an empty vector, because the only
/// consumer is an advisory field that must never fail a store.
///
/// # Normalization
///
/// The index stores dot products, not cosines — [`VectorIndex::add`]
/// requires normalized input and [`dot_product_simd`](crate::search::dot_product_simd)
/// divides by no norm. For a caller-supplied vector (a passthrough
/// namespace never validates one) an un-normalized query would produce
/// scores above 1.0 that look like similarities and are not. The check is
/// *skipped* rather than fixed up: normalizing here would either lie about
/// the stored vector or report a number the rest of the system disagrees
/// with, and an advisory's only value is that its number is trustworthy.
pub fn find_near_duplicates(
    index: &FlatVectorIndex,
    embedding: &[f32],
    namespace_id: NamespaceId,
    threshold: f32,
    limit: usize,
) -> Vec<(MemoryId, f32)> {
    if limit == 0 || index.len() == 0 {
        return Vec::new();
    }

    if !is_normalized(embedding, NORM_EPSILON) {
        tracing::debug!(
            %namespace_id,
            "skipping near-duplicate check: query vector is not L2-normalized"
        );
        if !UNNORMALIZED_WARNED.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                "near-duplicate detection is inert for un-normalized embeddings; \
                 scores would not be cosine similarities. This is logged once per process."
            );
        }
        return Vec::new();
    }

    let filter = SearchFilter {
        namespace_id: Some(namespace_id),
        // The same phases autolink scans, so the two similarity views of a
        // namespace stay comparable. Ghost has no summary left to show and
        // Tombstone is deleted; neither is something to be told it
        // duplicates.
        decay_phases: Some(vec![DecayPhase::Full.as_u8(), DecayPhase::Summary.as_u8()]),
        // Prunes the result heap, not the dot products — the scan is
        // O(index) either way.
        min_score: Some(threshold),
        ..SearchFilter::default()
    };

    match index.search(embedding, limit, &filter) {
        // Already descending by score.
        Ok(results) => results.into_iter().map(|r| (r.id, r.score)).collect(),
        Err(e) => {
            tracing::warn!(%e, "near-duplicate vector search failed (non-fatal)");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::constants::DUPLICATE_SIMILARITY_THRESHOLD;
    use crate::search::VectorMetadata;

    const NS: u32 = 1;
    const OTHER_NS: u32 = 2;

    fn threshold() -> f32 {
        DUPLICATE_SIMILARITY_THRESHOLD as f32
    }

    fn ns(id: u32) -> NamespaceId {
        NamespaceId::new(id)
    }

    /// Add a vector to the index and hand back its id.
    fn add(
        index: &mut FlatVectorIndex,
        vector: &[f32],
        namespace: u32,
        phase: DecayPhase,
    ) -> MemoryId {
        let id = MemoryId::new();
        index
            .add(
                id,
                vector,
                VectorMetadata {
                    namespace_id: ns(namespace),
                    decay_phase: phase.as_u8(),
                    tags: Vec::new(),
                },
            )
            .expect("add vector");
        id
    }

    /// An index holding one full-phase unit vector along the first axis.
    fn one_vector_index() -> (FlatVectorIndex, MemoryId) {
        let mut index = FlatVectorIndex::new(4);
        let id = add(&mut index, &[1.0, 0.0, 0.0, 0.0], NS, DecayPhase::Full);
        (index, id)
    }

    #[test]
    fn a1_an_empty_index_reports_nothing() {
        let index = FlatVectorIndex::new(4);
        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 3);
        assert!(found.is_empty());
    }

    #[test]
    fn a2_an_identical_vector_scores_one() {
        let (index, id) = one_vector_index();
        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 3);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0, id);
        assert!((found[0].1 - 1.0).abs() < 1e-5, "{}", found[0].1);
    }

    #[test]
    fn a3_a_vector_below_the_threshold_is_ignored() {
        let (index, _) = one_vector_index();
        // Orthogonal: cosine 0.0, far below 0.85.
        let found = find_near_duplicates(&index, &[0.0, 1.0, 0.0, 0.0], ns(NS), threshold(), 3);
        assert!(found.is_empty());
    }

    #[test]
    fn a4_the_scan_is_scoped_to_the_query_namespace() {
        let mut index = FlatVectorIndex::new(4);
        add(
            &mut index,
            &[1.0, 0.0, 0.0, 0.0],
            OTHER_NS,
            DecayPhase::Full,
        );
        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 3);
        assert!(found.is_empty());
    }

    #[test]
    fn a5_ghost_and_tombstone_memories_are_excluded() {
        let mut index = FlatVectorIndex::new(4);
        add(&mut index, &[1.0, 0.0, 0.0, 0.0], NS, DecayPhase::Ghost);
        add(&mut index, &[1.0, 0.0, 0.0, 0.0], NS, DecayPhase::Tombstone);
        let summary = add(&mut index, &[1.0, 0.0, 0.0, 0.0], NS, DecayPhase::Summary);

        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 5);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].0, summary);
    }

    #[test]
    fn a6_the_result_is_capped_at_the_limit() {
        let mut index = FlatVectorIndex::new(4);
        for _ in 0..5 {
            add(&mut index, &[1.0, 0.0, 0.0, 0.0], NS, DecayPhase::Full);
        }
        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 3);
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn a7_matches_come_back_in_descending_score_order() {
        let mut index = FlatVectorIndex::new(4);
        // Three unit vectors at increasing angles from the query.
        let far = add(&mut index, &[0.88, 0.475, 0.0, 0.0], NS, DecayPhase::Full);
        let exact = add(&mut index, &[1.0, 0.0, 0.0, 0.0], NS, DecayPhase::Full);
        let near = add(&mut index, &[0.96, 0.28, 0.0, 0.0], NS, DecayPhase::Full);

        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 5);
        let ids: Vec<MemoryId> = found.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, vec![exact, near, far], "{found:?}");
        assert!(found.windows(2).all(|w| w[0].1 >= w[1].1), "{found:?}");
    }

    /// The executable form of "guard the query, skip the check". `[2,0,0,0]`
    /// against a stored `[1,0,0,0]` has a dot product of 2.0 — a number that
    /// looks like a similarity, is above every threshold, and is not one.
    /// Reporting nothing is the correct answer; reporting 2.0 is not.
    #[test]
    fn a8_skips_the_check_when_the_query_vector_is_not_l2_normalized() {
        let (index, _) = one_vector_index();
        let found = find_near_duplicates(&index, &[2.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 3);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a9_a_dimension_mismatch_yields_no_matches_rather_than_an_error() {
        let (index, _) = one_vector_index();
        // Normalized, but eight-dimensional against a four-dimensional index.
        let query = [0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5, 0.5];
        let found = find_near_duplicates(&index, &query, ns(NS), threshold(), 3);
        assert!(found.is_empty());
    }

    #[test]
    fn a10_a_zero_limit_reports_nothing() {
        let (index, _) = one_vector_index();
        let found = find_near_duplicates(&index, &[1.0, 0.0, 0.0, 0.0], ns(NS), threshold(), 0);
        assert!(found.is_empty());
    }
}
