// Append to src/decay/sweep.rs in an isolated COPY of commit 211b1bd.
// These assert desired behavior and fail on that commit. No sleeps or races.
#[cfg(test)]
mod independent_review_regressions {
    use super::*;
    use crate::api::FsrsEngine as _;
    use crate::model::{AccessKind, NamespaceConfig};
    use crate::test_support::Fixture;
    const DAY: i64 = 86_400_000;

    fn adapter(f: &Fixture, now: i64) -> crate::api::adapters::FsrsEngineAdapter {
        crate::api::adapters::FsrsEngineAdapter::new(
            f.storage.clone(), f.cache.clone(), f.graph.clone(),
            Arc::new(crate::config::RecalldConfig::default()), false,
        ).with_clock(Arc::new(crate::time::ManualClock::new(now)))
    }

    #[tokio::test]
    async fn review_stale_sweep_must_not_overwrite_committed_feedback() {
        let f = Fixture::new();
        let mut ns = NamespaceConfig::default_namespace(0);
        ns.name = "review-feedback".into();
        ns.embedding_dim = crate::test_support::DIM as u32;
        ns.permastore_threshold = 2.0;
        let namespace = f.storage.write().unwrap().create_namespace(&ns).unwrap();
        let id = f.insert_memory_in(namespace, &[]).await;
        let (snapshot, errors) = DecaySweepRunner::load_metadata_batch(&f.storage.read().unwrap(), &[id]);
        assert!(errors.is_empty());
        let meta = snapshot.get(&id).unwrap();
        let decay = DecayConfig::default();
        let engine = FsrsEngine::new(&decay);
        let mut transitions = Vec::new();
        let mut result = SweepResult::default();
        let (raw, effective) = DecaySweepRunner::evaluate_record(
            id, DecayPhase::Full, meta, &engine, &ActivationConfig::default(),
            &f.graph, 8 * DAY, 1.0, &mut transitions, &mut result,
        ).await.unwrap();
        assert_eq!(transitions.len(), 1);
        // Deterministically insert feedback exactly between scan/plan and apply.
        let feedback = adapter(&f, 8 * DAY).reinforce(id, 3).await.unwrap();
        assert!(feedback.is_permastore);
        let committed = f.record(id).unwrap();
        DecaySweepRunner::update_record_state(
            id, DecayPhase::Full, raw, effective, meta, &f.storage, &f.graph, &mut result,
        ).await;
        DecaySweepRunner::flush_transitions(
            &transitions, &f.storage, &f.graph, &f.cache, &mut result,
        ).await;
        let after = f.record(id).unwrap();
        assert_eq!((after.stability, after.is_permastore, after.phase),
                   (committed.stability, committed.is_permastore, committed.phase));
    }

    #[tokio::test]
    async fn review_successful_sweep_must_publish_final_graph_phase() {
        let f = Fixture::new();
        let id = f.insert_memory().await;
        let sweep = adapter(&f, i64::MAX);
        for _ in 0..2 {
            let result = sweep.trigger_sweep(None).await.unwrap();
            assert!(result.errors.is_empty());
        }
        assert_eq!(f.record(id).unwrap().phase, DecayPhase::Ghost);
        assert_eq!(f.graph.read().await.get_node(&id).unwrap().decay_phase, DecayPhase::Ghost);
    }

    #[tokio::test]
    async fn review_access_must_not_move_feedback_anchor_backward() {
        let f = Fixture::new();
        let id = f.insert_memory().await;
        adapter(&f, 8 * DAY).reinforce(id, 3).await.unwrap();
        assert_eq!(f.record(id).unwrap().last_accessed_at, 8 * DAY);
        adapter(&f, DAY).record_access(id, AccessKind::DirectRetrieval).await;
        assert_eq!(f.record(id).unwrap().last_accessed_at, 8 * DAY);
    }
}
