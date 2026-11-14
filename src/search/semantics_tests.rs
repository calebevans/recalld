//! Synthetic engine fixtures using actual graph/vector/entity/RIF adapters.
use super::tests::{mid, ns, other_ns, query, rec, search_engine};
use super::*;
use crate::graph::{RelationshipGraph, SharedGraph};
use crate::model::{EdgeType, Tag};
use crate::search::adapters::{RifProcessorAdapter, SharedGraphReader, StorageMetadataAdapter};

fn graph_engine(
    records: Vec<CachedRecord>,
    vectors: &[(MemoryId, f32)],
    edges: &[(MemoryId, MemoryId, EdgeType)],
) -> (QueryEngine, SharedGraph) {
    let mut graph = RelationshipGraph::new();
    for record in &records {
        // Deliberately stale phase/strength: canonical records control v2 eligibility.
        graph
            .add_node(record.id, record.namespace_id, DecayPhase::Full, 1.0, 0)
            .unwrap();
    }
    for &(a, b, _) in edges {
        for id in [a, b] {
            if !graph.contains(&id) {
                graph.add_node(id, ns(), DecayPhase::Full, 1.0, 0).unwrap();
            }
        }
    }
    for &(a, b, kind) in edges {
        graph.add_edge(a, b, kind, 1.0, false).unwrap();
    }
    let graph = Arc::new(tokio::sync::RwLock::new(graph));
    let mut engine = search_engine(records, vectors);
    engine.graph = Arc::new(SharedGraphReader::new(graph.clone()));
    (engine, graph)
}

fn current() -> RetrievalSemantics {
    RetrievalSemantics::V2(VersionMode::Current)
}
fn history() -> RetrievalSemantics {
    RetrievalSemantics::V2(VersionMode::History)
}
fn result_ids(response: &SearchResponse) -> Vec<MemoryId> {
    response.results.iter().map(|r| r.memory_id).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_chain_boundaries_0_1_64_65_are_not_legacy_fallbacks() {
    for hops in [0, 1, 64, 65] {
        let records = (1..=hops + 1).map(|n| rec(mid(n))).collect();
        let edges: Vec<_> = (1..=hops)
            .map(|n| (mid(n + 1), mid(n), EdgeType::Supersedes))
            .collect();
        let (engine, _) = graph_engine(records, &[(mid(1), 0.9)], &edges);
        let result = engine
            .search_with_semantics(query(serde_json::json!({"text":"q"})), current())
            .await
            .unwrap();
        if hops <= 64 {
            assert_eq!(result_ids(&result.response), vec![mid(hops + 1)]);
            assert_eq!(
                result.diagnostics.versions[&mid(hops + 1)],
                VersionStatus::Current
            );
            assert_eq!(
                result.diagnostics.supersession_edges_examined,
                hops as usize
            );
            assert!((result.response.results[0].composite_score.unwrap() - 0.81).abs() < 1e-6);
        } else {
            assert!(result.response.results.is_empty());
            assert_eq!(
                result.diagnostics.unresolved[&UnresolvedReason::HopLimit],
                1
            );
            assert!(result.diagnostics.versions.is_empty());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_rejects_cycles_branches_missing_and_foreign_paths_but_history_retains_origin() {
    let cases = [
        (
            vec![(2, 1), (3, 2), (1, 3)],
            None,
            None,
            UnresolvedReason::Cycle,
        ),
        (vec![(2, 1), (3, 1)], None, None, UnresolvedReason::Branch),
        (vec![(2, 1)], Some(2), None, UnresolvedReason::MissingRecord),
        (
            vec![(2, 1)],
            None,
            Some(2),
            UnresolvedReason::ForeignNamespace,
        ),
        (
            vec![(2, 1), (3, 2)],
            None,
            Some(2),
            UnresolvedReason::ForeignNamespace,
        ),
    ];
    for (links, missing, foreign, reason) in cases {
        let mut records: Vec<_> = (1..=3)
            .filter(|n| Some(*n) != missing)
            .map(|n| rec(mid(n)))
            .collect();
        for record in &mut records {
            if Some(record.id) == foreign.map(mid) {
                record.namespace_id = other_ns();
            }
        }
        let edges: Vec<_> = links
            .into_iter()
            .map(|(a, b)| (mid(a), mid(b), EdgeType::Supersedes))
            .collect();
        let (engine, graph) = graph_engine(records, &[(mid(1), 0.9)], &edges);
        // Stale graph scope cannot override the canonical foreign intermediate.
        if let Some(id) = foreign {
            let mut g = graph.write().await;
            let key = g.id_index[&mid(id)];
            g.nodes[key].namespace_id = ns();
        }
        let q = serde_json::json!({"text":"q"});
        let strict = engine
            .search_with_semantics(query(q.clone()), current())
            .await
            .unwrap();
        assert!(strict.response.results.is_empty(), "{reason:?}");
        assert_eq!(strict.diagnostics.unresolved[&reason], 1);
        assert!(strict.diagnostics.versions.is_empty());
        let retained = engine
            .search_with_semantics(query(q.clone()), history())
            .await
            .unwrap();
        assert_eq!(result_ids(&retained.response), vec![mid(1)]);
        assert_eq!(
            retained.diagnostics.versions[&mid(1)],
            VersionStatus::Unresolved(reason)
        );
        let legacy = engine.search(query(q)).await.unwrap();
        assert_eq!(legacy.results.len(), 1); // Existing fallback/chain behavior stays v1.
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn filtered_successor_is_omitted_in_current_without_ineligible_diagnostic_ids() {
    for failure in 0..7 {
        let mut old = rec(mid(1));
        old.tags = vec![Tag::new("keep").unwrap()];
        old.is_permastore = true;
        let mut successor = old.clone();
        successor.id = mid(2);
        match failure {
            0 => successor.phase = DecayPhase::Tombstone,
            1 => successor.phase = DecayPhase::Ghost,
            2 => successor.phase = DecayPhase::Summary,
            3 => successor.strength = 0.1,
            4 => successor.tags.clear(),
            5 => successor.tags.push(Tag::new("blocked").unwrap()),
            _ => successor.is_permastore = false,
        }
        let (engine, _) = graph_engine(
            vec![old, successor],
            &[(mid(1), 0.9)],
            &[(mid(2), mid(1), EdgeType::Supersedes)],
        );
        let q = serde_json::json!({"text":"q","filter":{"phases":["full"],"minStrength":0.5,"requireTags":["keep"],"excludeTags":["blocked"],"permastoreOnly":true}});
        let strict = engine
            .search_with_semantics(query(q.clone()), current())
            .await
            .unwrap();
        assert!(strict.response.results.is_empty());
        assert_eq!(
            strict.diagnostics.unresolved[&UnresolvedReason::Ineligible],
            1
        );
        assert!(strict.diagnostics.versions.is_empty());
        let retained = engine
            .search_with_semantics(query(q.clone()), history())
            .await
            .unwrap();
        assert_eq!(result_ids(&retained.response), vec![mid(1)]);
        assert_eq!(
            retained.diagnostics.versions[&mid(1)],
            VersionStatus::Historical
        );
        assert_eq!(
            result_ids(&engine.search(query(q)).await.unwrap()),
            vec![mid(1)]
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tombstone_can_resolve_structural_chain_but_never_inherits_or_returns_content() {
    let mut tombstone = rec(mid(2));
    tombstone.phase = DecayPhase::Tombstone;
    tombstone.strength = 0.0;
    let (engine, _) = graph_engine(
        vec![rec(mid(1)), tombstone, rec(mid(3))],
        &[(mid(1), 0.9), (mid(2), 1.0), (mid(3), 0.1)],
        &[
            (mid(2), mid(1), EdgeType::Supersedes),
            (mid(3), mid(2), EdgeType::Supersedes),
        ],
    );
    let q = serde_json::json!({"text":"q","includeGhosts":true});
    let strict = engine
        .search_with_semantics(query(q.clone()), current())
        .await
        .unwrap();
    assert_eq!(result_ids(&strict.response), vec![mid(3)]);
    assert!((strict.response.results[0].composite_score.unwrap() - 0.81).abs() < 1e-6);
    assert_eq!(strict.response.results[0].score, Some(0.1));
    let retained = engine
        .search_with_semantics(query(q), history())
        .await
        .unwrap();
    assert_eq!(result_ids(&retained.response), vec![mid(1), mid(3)]);
    assert!((retained.response.results[1].composite_score.unwrap() - 0.09).abs() < 1e-6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dense_supersession_reads_share_one_query_budget_in_uuid_order() {
    let mut edges = Vec::new();
    for root in [1, 2] {
        for n in 0..8000 {
            edges.push((mid(100 + root * 8000 + n), mid(root), EdgeType::Associative));
        }
    }
    for reverse in [false, true] {
        if reverse {
            edges.reverse();
        }
        let (engine, _) = graph_engine(
            vec![rec(mid(2)), rec(mid(1))],
            &[(mid(2), 0.9), (mid(1), 0.9)],
            &edges,
        );
        let result = engine
            .search_with_semantics(query(serde_json::json!({"text":"q"})), current())
            .await
            .unwrap();
        assert_eq!(result_ids(&result.response), vec![mid(1)]);
        assert_eq!(result.diagnostics.supersession_edges_examined, 12_800);
        assert_eq!(
            result.diagnostics.unresolved[&UnresolvedReason::WorkLimit],
            1
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_depth_is_an_actual_bound_and_replacement_seed_evidence_is_invariant() {
    let edges = [
        (mid(1), mid(2), EdgeType::Associative),
        (mid(2), mid(3), EdgeType::Associative),
        (mid(3), mid(4), EdgeType::Associative),
    ];
    let (engine, _) = graph_engine(
        (1..=4).map(|n| rec(mid(n))).collect(),
        &[(mid(1), 1.0)],
        &edges,
    );
    for depth in 0..=3 {
        let result = engine
            .search_with_semantics(
                query(serde_json::json!({"text":"q","graphDepth":depth})),
                history(),
            )
            .await
            .unwrap();
        let ids: HashSet<_> = result_ids(&result.response).into_iter().collect();
        assert_eq!(ids, (1..=depth + 1).map(mid).collect());
        for n in 2..=depth + 1 {
            assert_eq!(result.diagnostics.graph_depths[&mid(n)], (n - 1) as u8);
        }
    }
    let edges = [
        (mid(2), mid(1), EdgeType::Supersedes),
        (mid(2), mid(3), EdgeType::Associative),
    ];
    let mut snapshots = Vec::new();
    for own in [None, Some(0.1)] {
        let mut vectors = vec![(mid(1), 0.9)];
        if let Some(score) = own {
            vectors.push((mid(2), score));
        }
        let (engine, _) = graph_engine((1..=3).map(|n| rec(mid(n))).collect(), &vectors, &edges);
        let result = engine
            .search_with_semantics(
                query(serde_json::json!({"text":"q","graphDepth":1})),
                current(),
            )
            .await
            .unwrap();
        snapshots.push(
            result
                .response
                .results
                .iter()
                .map(|r| (r.memory_id, r.composite_score))
                .collect::<Vec<_>>(),
        );
        assert_eq!(result.diagnostics.graph_depths[&mid(3)], 1);
    }
    assert_eq!(snapshots[0], snapshots[1]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_cannot_receive_or_relay_through_ghost_tombstone_filtered_or_foreign_records() {
    for failure in 0..5 {
        let mut records: Vec<_> = (1..=3)
            .map(|n| {
                let mut r = rec(mid(n));
                r.tags = vec![Tag::new("keep").unwrap()];
                r
            })
            .collect();
        match failure {
            0 => records[1].phase = DecayPhase::Ghost,
            1 => records[1].phase = DecayPhase::Tombstone,
            2 => records[1].tags.clear(),
            3 => records[1].strength = 0.1,
            _ => records[1].namespace_id = other_ns(),
        }
        let (engine, _) = graph_engine(
            records,
            &[(mid(1), 1.0)],
            &[
                (mid(1), mid(2), EdgeType::Associative),
                (mid(2), mid(3), EdgeType::Associative),
            ],
        );
        for mode in [current(), history()] {
            let result=engine.search_with_semantics(query(serde_json::json!({"text":"q","graphDepth":3,"includeGhosts":true,"filter":{"requireTags":["keep"],"minStrength":0.5}})),mode).await.unwrap();
            assert_eq!(
                result_ids(&result.response),
                vec![mid(1)],
                "failure {failure}"
            );
            assert_eq!(
                result.diagnostics.graph_depths,
                std::collections::BTreeMap::from([(mid(1), 0)])
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_resolves_before_graph_seeding_and_after_graph_discovery() {
    let edges = [
        (mid(2), mid(1), EdgeType::Supersedes),
        (mid(1), mid(3), EdgeType::Associative),
        (mid(2), mid(4), EdgeType::Associative),
        (mid(5), mid(4), EdgeType::Supersedes),
    ];
    let (engine, _) = graph_engine(
        (1..=5).map(|n| rec(mid(n))).collect(),
        &[(mid(1), 0.9)],
        &edges,
    );
    let result = engine
        .search_with_semantics(
            query(serde_json::json!({"text":"q","graphDepth":1})),
            current(),
        )
        .await
        .unwrap();
    assert_eq!(result_ids(&result.response), vec![mid(2), mid(5)]);
    assert_eq!(result.diagnostics.graph_depths[&mid(5)], 1);
    assert!(!result.diagnostics.versions.contains_key(&mid(4)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diamond_cycle_contributions_and_caps_are_deterministic() {
    use crate::graph::query::expand;
    let mut edges = vec![
        (mid(1), mid(2), EdgeType::Associative),
        (mid(1), mid(3), EdgeType::Associative),
        (mid(2), mid(4), EdgeType::Associative),
        (mid(3), mid(4), EdgeType::Associative),
        (mid(4), mid(5), EdgeType::Associative),
        (mid(5), mid(2), EdgeType::Associative),
        (mid(1), mid(6), EdgeType::Contradicts),
    ];
    let mut snapshots = Vec::new();
    for reverse in [false, true] {
        let mut records: Vec<_> = (1..=6).map(|n| rec(mid(n))).collect();
        if reverse {
            edges.reverse();
            records.reverse();
        }
        let (engine, _) = graph_engine(records, &[(mid(1), 0.9)], &edges);
        let result = engine
            .search_with_semantics(
                query(serde_json::json!({"text":"q","graphDepth":3})),
                history(),
            )
            .await
            .unwrap();
        assert!(!result_ids(&result.response).contains(&mid(6)));
        snapshots.push((
            result
                .response
                .results
                .iter()
                .map(|r| (r.memory_id, r.composite_score))
                .collect::<Vec<_>>(),
            result.diagnostics.graph_depths,
        ));
    }
    assert_eq!(snapshots[0], snapshots[1]);
    let records: Vec<_> = (1..=802).map(|n| rec(mid(n))).collect();
    let eligible: HashMap<_, _> = records.iter().map(|r| (r.id, r.clone())).collect();
    let edges: Vec<_> = (1..=401)
        .map(|n| (mid(n), mid(n + 401), EdgeType::Associative))
        .collect();
    let (_, graph) = graph_engine(records, &[], &edges);
    let seeds: Vec<_> = (1..=401).rev().map(|n| (mid(n), 1.0)).collect();
    let expanded = expand(&*graph.read().await, &seeds, ns(), 1, &eligible).unwrap();
    assert_eq!(expanded.work.expanded_states, 400);
    assert!(expanded.work.state_limit);
    assert_eq!(expanded.hits.len(), 400);
    assert!(!expanded.hits.iter().any(|r| r.id == mid(802)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dense_graph_edge_budget_reports_incomplete_without_partial_adjacency_bias() {
    use crate::graph::query::expand;
    let records = vec![rec(mid(1))];
    let edges: Vec<_> = (2..=12802)
        .map(|n| (mid(n), mid(1), EdgeType::Associative))
        .collect();
    let eligible = HashMap::from([(mid(1), rec(mid(1)))]);
    let (_, graph) = graph_engine(records, &[], &edges);
    let expanded = expand(&*graph.read().await, &[(mid(1), 1.0)], ns(), 3, &eligible).unwrap();
    assert_eq!(expanded.work.examined_edges, 12800);
    assert!(expanded.work.edge_limit);
    assert!(expanded.hits.is_empty());
}

struct NoRif;
impl RifProcessor for NoRif {
    fn compute_suppressions(
        &self,
        _retrieved: &[MemoryId],
        _neighbors: &[MemoryId],
    ) -> Vec<RifSuppression> {
        panic!("semantics2 must never invoke legacy RIF")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_rif_changes_legacy_scores_but_v2_is_off_and_neither_persists_suppression() {
    use crate::storage::StorageEngine as _;
    use crate::test_support::Fixture;
    let fixture = Fixture::new();
    let a = fixture.insert_memory().await;
    let b = fixture.insert_memory().await;
    fixture
        .graph
        .write()
        .await
        .add_edge(a, b, EdgeType::Associative, 1.0, false)
        .unwrap();
    let mut engine = search_engine(Vec::new(), &[]);
    engine.meta_store = Arc::new(StorageMetadataAdapter::new(fixture.storage.clone()));
    engine.graph = Arc::new(SharedGraphReader::new(fixture.graph.clone()));
    engine.rif_engine = Arc::new(RifProcessorAdapter::new(
        Arc::new(crate::rif::RifEngine::new(crate::rif::RifConfig::default())),
        fixture.graph.clone(),
    ));
    // The fixture access recorder is a no-op, isolating RIF from legacy exposure writes.
    let before: Vec<_> = [a, b]
        .map(|id| fixture.record(id).unwrap().to_bytes())
        .into();
    let q = serde_json::json!({"queryMode":"metadata_only","filter":{"minStrength":0.0}});
    let legacy = engine.search(query(q.clone())).await.unwrap();
    assert!(
        legacy
            .results
            .iter()
            .all(|r| r.effective_r < 1.0 && r.composite_score.unwrap() < 0.1)
    );
    engine.rif_engine = Arc::new(NoRif);
    for mode in [current(), history()] {
        let result = engine
            .search_with_semantics(query(q.clone()), mode)
            .await
            .unwrap();
        assert!(result.diagnostics.rif_off);
        assert!(
            result
                .response
                .results
                .iter()
                .all(|r| r.effective_r == 1.0 && r.composite_score == Some(0.1))
        );
    }
    tokio::task::yield_now().await;
    assert_eq!(
        before,
        vec![
            fixture.record(a).unwrap().to_bytes(),
            fixture.record(b).unwrap().to_bytes()
        ]
    );
    // A real canonical mutation between queries must affect the next resolution.
    fixture.storage.read().unwrap().tombstone_memory(b).unwrap();
    let result = engine
        .search_with_semantics(query(q), current())
        .await
        .unwrap();
    assert_eq!(result_ids(&result.response), vec![a]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_rejects_nonfinite_inputs_and_scores_and_transport_json_cannot_select_it() {
    let (engine, _) = graph_engine(vec![rec(mid(1))], &[(mid(1), 0.9)], &[]);
    let mut q = query(serde_json::json!({"text":"q"}));
    q.min_score = f32::NAN;
    assert!(engine.search_with_semantics(q, current()).await.is_err());
    let (engine, _) = graph_engine(vec![rec(mid(1))], &[(mid(1), f32::NAN)], &[]);
    assert!(
        engine
            .search_with_semantics(query(serde_json::json!({"text":"q"})), current())
            .await
            .is_err()
    );
    let mut successor = rec(mid(2));
    successor.phase = DecayPhase::Tombstone;
    let (engine, _) = graph_engine(
        vec![rec(mid(1)), successor],
        &[(mid(1), 0.9)],
        &[(mid(2), mid(1), EdgeType::Supersedes)],
    );
    let legacy = engine
        .search(query(
            serde_json::json!({"text":"q","retrievalSemanticsVersion":2,"versionMode":"current"}),
        ))
        .await
        .unwrap();
    assert_eq!(result_ids(&legacy), vec![mid(1)]);
    assert_eq!(RetrievalSemantics::default(), RetrievalSemantics::V1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_requires_structure_and_correct_scope_even_when_the_canonical_record_is_eligible() {
    for missing in [false, true] {
        let (engine, graph) = graph_engine(vec![rec(mid(1))], &[(mid(1), 0.9)], &[]);
        {
            let mut graph = graph.write().await;
            if missing {
                graph.remove_memory(mid(1));
            } else {
                let key = graph.id_index[&mid(1)];
                graph.nodes[key].namespace_id = other_ns();
            }
        }
        let result = engine
            .search_with_semantics(query(serde_json::json!({"text":"q"})), current())
            .await
            .unwrap();
        assert!(result.response.results.is_empty());
        let reason = if missing {
            UnresolvedReason::MissingStructure
        } else {
            UnresolvedReason::ForeignNamespace
        };
        assert_eq!(result.diagnostics.unresolved[&reason], 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_inheritance_preserves_min_score_temporal_boost_and_stronger_own_evidence() {
    for own in [None, Some(0.1), Some(0.95)] {
        let mut vectors = vec![(mid(1), 0.9)];
        if let Some(score) = own {
            vectors.push((mid(2), score));
        }
        let (engine, _) = graph_engine(
            vec![rec(mid(1)), rec(mid(2))],
            &vectors,
            &[(mid(2), mid(1), EdgeType::Supersedes)],
        );
        let result = engine
            .search_with_semantics(
                query(serde_json::json!({"text":"q","minScore":0.8,
            "timeRangeStart":1_699_999_999_000i64,"timeRangeEnd":1_700_000_001_000i64})),
                current(),
            )
            .await
            .unwrap();
        assert_eq!(result_ids(&result.response), vec![mid(2)]);
        let expected = own.unwrap_or(0.0).max(0.9) * 0.9 * 1.5;
        assert!((result.response.results[0].composite_score.unwrap() - expected).abs() < 1e-6);
        assert_eq!(result.response.total_matches, 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_metadata_budget_uses_ranked_eligible_records_and_uuid_ties() {
    let records: Vec<_> = (1..=110).rev().map(|n| rec(mid(n))).collect();
    let (engine, _) = graph_engine(records, &[], &[]);
    let result = engine
        .search_with_semantics(
            query(serde_json::json!({"queryMode":"metadata_only",
        "limit":2,"filter":{"minStrength":0.0}})),
            history(),
        )
        .await
        .unwrap();
    assert_eq!(result_ids(&result.response), vec![mid(1), mid(2)]);
    assert_eq!(result.response.total_matches, 100);
    assert!(result.diagnostics.metadata_candidate_limited);
    assert!(result.diagnostics.result_limit_applied);
    assert_eq!(result.diagnostics.versions.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_rejects_nonfinite_canonical_and_graph_sources() {
    let mut bad = rec(mid(1));
    bad.decay_strength = f32::NAN;
    let (engine, _) = graph_engine(vec![bad], &[(mid(1), 0.9)], &[]);
    assert!(matches!(
        engine
            .search_with_semantics(query(serde_json::json!({"text":"q"})), current())
            .await,
        Err(SearchError::MetadataError(_))
    ));
    let (engine, graph) = graph_engine(
        vec![rec(mid(1)), rec(mid(2))],
        &[(mid(1), 0.9)],
        &[(mid(1), mid(2), EdgeType::Associative)],
    );
    {
        let mut graph = graph.write().await;
        for (_, edge) in graph.edges.iter_mut() {
            edge.weight = f32::NAN;
        }
    }
    assert!(
        engine
            .search_with_semantics(
                query(serde_json::json!({"text":"q","graphDepth":1})),
                history()
            )
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_refill_uses_canonical_filters_before_cap_and_keeps_v1_behavior() {
    use crate::search::{FtsIndex, adapters::SharedFtsIndexRegistry};
    let dir = tempfile::tempdir().unwrap();
    let index = FtsIndex::new(dir.path()).unwrap();
    let mut records = Vec::new();
    for n in (1..=305).rev() {
        index.add(ns(), mid(n), "keyword", None, &[]).unwrap();
        let mut record = rec(mid(n));
        record.tags = vec![Tag::new("keep").unwrap()];
        if n <= 300 {
            match n % 6 {
                0 => continue, // stale index hit, missing canonical record
                1 => record.namespace_id = other_ns(),
                2 => record.phase = DecayPhase::Tombstone,
                3 => record.phase = DecayPhase::Ghost,
                4 => record.strength = 0.1,
                _ => record.tags.clear(),
            }
        }
        records.push(record);
    }
    let (mut engine, _) = graph_engine(records, &[], &[]);
    engine.fts_index = Arc::new(SharedFtsIndexRegistry::new(Arc::new(
        tokio::sync::Mutex::new(index),
    )));
    let q = serde_json::json!({"text":"ignored", "ftsQuery":"keyword", "limit":5,
        "filter":{"requireTags":["keep"],"minStrength":0.5}});
    for mode in [current(), history()] {
        let result = engine
            .search_with_semantics(query(q.clone()), mode)
            .await
            .unwrap();
        assert_eq!(
            result_ids(&result.response),
            (301..=305).map(mid).collect::<Vec<_>>()
        );
        assert_eq!(
            result.diagnostics.fts.availability,
            FtsAvailability::Available
        );
        let coverage = result.diagnostics.fts.coverage.unwrap();
        assert_eq!(
            (
                coverage.examined,
                coverage.eligible,
                coverage.admitted,
                coverage.pages
            ),
            (305, 5, 5, 4)
        );
        assert!(coverage.exhausted);
        assert!(!coverage.candidate_limited);
        assert!(!coverage.budget_exhausted);
    }
    let legacy = engine
        .search_with_semantics(query(q), RetrievalSemantics::V1)
        .await
        .unwrap();
    assert!(legacy.response.results.is_empty());
    assert_eq!(
        legacy.diagnostics.fts.availability,
        FtsAvailability::Available
    );
    assert!(legacy.diagnostics.fts.coverage.is_none()); // Top-B cannot prove exhaustion.
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_sqlite_failure_propagates_in_v2_and_labels_legacy_fallback() {
    use crate::search::{FtsIndex, adapters::SharedFtsIndexRegistry};
    let dir = tempfile::tempdir().unwrap();
    let index = FtsIndex::new(dir.path()).unwrap();
    let connection = rusqlite::Connection::open(dir.path().join("fts.db")).unwrap();
    connection.execute("DROP TABLE id_map", []).unwrap();
    let adapter = Arc::new(SharedFtsIndexRegistry::new(Arc::new(
        tokio::sync::Mutex::new(index),
    )));
    assert!(adapter.search(ns(), "keyword", 100).is_err());
    assert!(
        adapter
            .search_eligible(ns(), "keyword", 100, &HashSet::new())
            .is_err()
    );
    for vectors in [vec![], vec![(mid(1), 0.9)]] {
        let (mut engine, _) = graph_engine(vec![rec(mid(1))], &vectors, &[]);
        engine.fts_index = adapter.clone();
        let q = serde_json::json!({"text":"keyword"});
        for mode in [current(), history()] {
            let error = engine
                .search_with_semantics(query(q.clone()), mode)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("FTS search failed"));
        }
        let legacy = engine
            .search_with_semantics(query(q), RetrievalSemantics::V1)
            .await
            .unwrap();
        assert_eq!(legacy.response.results.len(), vectors.len());
        assert_eq!(
            legacy.diagnostics.fts.availability,
            FtsAvailability::Unavailable
        );
        assert!(legacy.diagnostics.fts.coverage.is_none());
        let metadata = engine
            .search_with_semantics(
                query(serde_json::json!({
                    "queryMode":"metadata_only", "filter":{"minStrength":0.0}
                })),
                current(),
            )
            .await
            .unwrap();
        assert_eq!(
            metadata.diagnostics.fts.availability,
            FtsAvailability::NotRequested
        );
        assert!(metadata.diagnostics.fts.coverage.is_none());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_engine_reports_work_underfill_and_candidate_cap_separately() {
    use crate::search::{FtsIndex, MAX_FTS_EXAMINED, adapters::SharedFtsIndexRegistry};
    let dir = tempfile::tempdir().unwrap();
    let index = FtsIndex::new(dir.path()).unwrap();
    // All index hits before the last one lack canonical records.
    for n in 1..=MAX_FTS_EXAMINED + 1 {
        index
            .add(ns(), mid(n as u128), "keyword", None, &[])
            .unwrap();
    }
    let adapter = Arc::new(SharedFtsIndexRegistry::new(Arc::new(
        tokio::sync::Mutex::new(index),
    )));
    let (mut engine, _) = graph_engine(vec![rec(mid(MAX_FTS_EXAMINED as u128 + 1))], &[], &[]);
    engine.fts_index = adapter.clone();
    let q = serde_json::json!({"text":"keyword", "limit":1});
    let underfill = engine
        .search_with_semantics(query(q.clone()), current())
        .await
        .unwrap();
    assert!(underfill.response.results.is_empty());
    let coverage = underfill.diagnostics.fts.coverage.unwrap();
    assert_eq!(coverage.examined, MAX_FTS_EXAMINED);
    assert_eq!(coverage.admitted, 0);
    assert!(coverage.budget_exhausted);
    assert!(!coverage.exhausted);
    assert!(coverage.candidate_limited);
    assert!(!underfill.diagnostics.result_limit_applied);

    let (mut engine, _) = graph_engine((1..=200).map(|n| rec(mid(n))).collect(), &[], &[]);
    engine.fts_index = adapter;
    let capped = engine
        .search_with_semantics(query(q), current())
        .await
        .unwrap();
    let coverage = capped.diagnostics.fts.coverage.unwrap();
    assert_eq!(
        (coverage.examined, coverage.eligible, coverage.admitted),
        (101, 101, 100)
    );
    assert!(!coverage.budget_exhausted);
    assert!(!coverage.exhausted);
    assert!(coverage.candidate_limited);
    assert!(capped.diagnostics.result_limit_applied);
    assert_eq!(capped.response.total_matches, 100);
    assert_eq!(result_ids(&capped.response), vec![mid(1)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuilt_reciprocal_edges_each_contribute_and_survive_engine_recall() {
    use crate::graph::{SpreadingActivationConfig, query::expand, rebuild_from_storage};
    use crate::storage::PersistedEdge;
    let config = SpreadingActivationConfig::default();
    let mut records = vec![rec(mid(1)), rec(mid(2))];
    for record in &mut records {
        record.strength = 0.9;
    }
    let eligible: HashMap<_, _> = records.iter().map(|r| (r.id, r.clone())).collect();
    let edges = [
        PersistedEdge {
            source: mid(1),
            target: mid(2),
            edge_type: EdgeType::Associative,
            weight: 0.01,
            auto_created: false,
            created_at: 0,
        },
        PersistedEdge {
            source: mid(2),
            target: mid(1),
            edge_type: EdgeType::Associative,
            weight: 0.8,
            auto_created: false,
            created_at: 0,
        },
    ];
    let expected = 0.9 * (0.01 + 0.8) * (config.s_max - 3.0_f32.ln()) * config.hop_decay * 0.9;
    assert!(expected > config.output_threshold);
    let mut reference = None;
    for order in [[0, 1], [1, 0]] {
        let (engine, graph) = graph_engine(records.clone(), &[(mid(1), 1.0)], &[]);
        {
            let mut graph = graph.write().await;
            assert_eq!(
                rebuild_from_storage(&mut graph, order.into_iter().map(|i| edges[i].clone())),
                2
            );
        }
        let expansion = expand(&*graph.read().await, &[(mid(1), 1.0)], ns(), 1, &eligible).unwrap();
        assert_eq!(expansion.hits.len(), 1);
        assert_eq!((expansion.hits[0].id, expansion.hits[0].depth), (mid(2), 1));
        assert!((expansion.hits[0].activation - expected).abs() < 1e-6);
        assert_eq!(expansion.work.expanded_states, 1);
        assert_eq!(expansion.work.examined_edges, 2);
        assert!(!expansion.work.edge_limit);
        assert!(!expansion.work.state_limit);
        if let Some(bits) = reference {
            assert_eq!(expansion.hits[0].activation.to_bits(), bits);
        }
        reference = Some(expansion.hits[0].activation.to_bits());
        let response = engine
            .search_with_semantics(
                query(serde_json::json!({
                    "text":"q", "graphDepth":1
                })),
                current(),
            )
            .await
            .unwrap();
        assert_eq!(result_ids(&response.response), vec![mid(1), mid(2)]);
        assert_eq!(response.diagnostics.graph_depths[&mid(2)], 1);
        assert_eq!(response.diagnostics.graph.examined_edges, 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuilt_reciprocal_edge_ties_use_uuid_and_charge_each_adjacency() {
    use crate::graph::{SpreadingActivationConfig, query::expand, rebuild_from_storage};
    use crate::storage::PersistedEdge;
    let config = SpreadingActivationConfig::default();
    let mut records = vec![rec(mid(1)), rec(mid(2)), rec(mid(3))];
    for record in &mut records {
        record.strength = 0.9;
    }
    let eligible: HashMap<_, _> = records.iter().map(|r| (r.id, r.clone())).collect();
    let mut edges = Vec::new();
    for target in [mid(3), mid(2)] {
        for (source, target, weight) in [(mid(1), target, 0.1), (target, mid(1), 0.8)] {
            edges.push(PersistedEdge {
                source,
                target,
                edge_type: EdgeType::Associative,
                weight,
                auto_created: false,
                created_at: 0,
            });
        }
    }
    for reverse in [false, true] {
        if reverse {
            edges.reverse();
        }
        let (_, graph) = graph_engine(records.clone(), &[], &[]);
        assert_eq!(
            rebuild_from_storage(&mut *graph.write().await, edges.clone().into_iter()),
            4
        );
        let expansion = expand(
            &*graph.read().await,
            &[(mid(1), 1.0), (mid(1), 0.5)],
            ns(),
            1,
            &eligible,
        )
        .unwrap();
        assert_eq!(
            expansion.hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![mid(2), mid(3)]
        );
        let expected = 0.9 * (0.1 + 0.8) * (config.s_max - 5.0_f32.ln()) * config.hop_decay * 0.9;
        assert!((expansion.hits[0].activation - expected).abs() < 1e-6);
        assert_eq!(
            expansion.hits[0].activation.to_bits(),
            expansion.hits[1].activation.to_bits()
        );
        assert_eq!(expansion.work.expanded_states, 1); // Duplicate seed is not reinjected.
        assert_eq!(expansion.work.examined_edges, 4); // Both stored directions count.
    }
}
