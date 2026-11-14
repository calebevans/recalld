//! Actual QueryEngine/graph/vector/FTS paths, with deterministic canonical fixtures.
use super::tests::{mid, ns, other_ns, query, rec, search_engine};
use super::*;
use crate::graph::{RelationshipGraph, SharedGraph};
use crate::model::{EdgeType, Tag};
use crate::search::adapters::{SharedFtsIndexRegistry, SharedGraphReader};
use serde_json::json;

fn history() -> RetrievalSemantics {
    RetrievalSemantics::V2(VersionMode::History)
}
fn current() -> RetrievalSemantics {
    RetrievalSemantics::V2(VersionMode::Current)
}
fn metadata(limit: usize) -> SearchQuery {
    query(
        json!({"queryMode":"metadata_only","limit":limit,"graphDepth":0,"recordAccess":false,"filter":{"minStrength":0.0}}),
    )
}
fn fixture(
    records: Vec<CachedRecord>,
    vectors: &[(MemoryId, f32)],
    edges: &[(u128, u128, EdgeType, f32)],
) -> (QueryEngine, SharedGraph) {
    let mut graph = RelationshipGraph::new();
    for r in &records {
        graph
            .add_node(r.id, r.namespace_id, DecayPhase::Full, 0.01, 0)
            .unwrap();
    }
    crate::graph::rebuild_from_storage(
        &mut graph,
        edges
            .iter()
            .map(|&(a, b, kind, weight)| crate::storage::PersistedEdge {
                source: mid(a),
                target: mid(b),
                edge_type: kind,
                weight,
                auto_created: false,
                created_at: 0,
            }),
    );
    let graph = Arc::new(tokio::sync::RwLock::new(graph));
    let mut engine = search_engine(records, vectors);
    engine.graph = Arc::new(SharedGraphReader::new(graph.clone()));
    (engine, graph)
}
fn values(r: &SemanticsResponse) -> Vec<(MemoryId, u32, u32)> {
    r.response
        .results
        .iter()
        .map(|v| {
            (
                v.memory_id,
                v.composite_score.unwrap().to_bits(),
                v.effective_r.to_bits(),
            )
        })
        .collect()
}
fn hit(r: &SemanticsResponse, id: u128) -> &SearchResult {
    r.response
        .results
        .iter()
        .find(|r| r.memory_id == mid(id))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_modes_keep_shadow_bit_identical_and_v1_legacy_independent() {
    let (mut e, _) = fixture(
        vec![rec(mid(1)), rec(mid(2))],
        &[],
        &[(1, 2, EdgeType::Associative, 1.0)],
    );
    let off = e
        .search_with_semantics(metadata(2), history())
        .await
        .unwrap();
    assert!(off.diagnostics.rif.is_none());
    e.rif_mode = RifMode::Shadow;
    let shadow = e
        .search_with_semantics(metadata(2), history())
        .await
        .unwrap();
    assert_eq!(values(&off), values(&shadow));
    assert_eq!(shadow.diagnostics.rif.as_ref().unwrap().admitted_pairs, 2);
    assert_eq!(
        shadow.diagnostics.rif.as_ref().unwrap().suppressed_targets,
        2
    );
    e.rif_mode = RifMode::Experimental;
    let experiment = e
        .search_with_semantics(metadata(2), history())
        .await
        .unwrap();
    assert!(hit(&experiment, 2).effective_r < hit(&off, 2).effective_r);
    assert!(!experiment.diagnostics.rif_off);
    let v1 = e
        .search_with_semantics(metadata(2), RetrievalSemantics::V1)
        .await
        .unwrap();
    e.rif_mode = RifMode::Off;
    let legacy = e
        .search_with_semantics(metadata(2), RetrievalSemantics::V1)
        .await
        .unwrap();
    assert_eq!(values(&v1), values(&legacy));
    assert!(v1.diagnostics.rif.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_parallel_reciprocal_duplicate_and_self_edges_are_deduplicated_before_caps() {
    let records = vec![rec(mid(1)), rec(mid(2))];
    let (base, _) = fixture(records.clone(), &[], &[(1, 2, EdgeType::Associative, 1.0)]);
    let expected = base
        .with_rif_mode(RifMode::Experimental)
        .search_with_semantics(metadata(2), history())
        .await
        .unwrap();
    let mut edges = vec![
        (1, 2, EdgeType::Associative, 0.1),
        (1, 2, EdgeType::Associative, 1.0),
        (2, 1, EdgeType::Contradicts, 1.0),
        (1, 2, EdgeType::Causal, 1.0),
        (1, 1, EdgeType::Associative, 1.0),
    ];
    for _ in 0..2 {
        edges.reverse();
        let (e, _) = fixture(records.clone(), &[], &edges);
        let got = e
            .with_rif_mode(RifMode::Experimental)
            .search_with_semantics(metadata(2), history())
            .await
            .unwrap();
        assert_eq!(values(&got), values(&expected));
        let d = got.diagnostics.rif.unwrap();
        assert_eq!(d.eligible_pairs, 2);
        assert_eq!(d.admitted_pairs, 2);
        assert_eq!(d.duplicate_pairs, 6);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_caps_count_unique_eligible_pairs_and_do_not_depend_on_edge_order() {
    for (anchors, targets, expected, anchor_hits, pair_hit) in [
        (1, 100, 100, 0, false),
        (1, 101, 100, 1, false),
        (10, 100, 1000, 0, false),
        (11, 100, 1000, 0, true),
        (11, 101, 1000, 11, true),
    ] {
        let mut records = Vec::new();
        let mut edges = Vec::new();
        for a in 1..=anchors {
            let mut anchor = rec(mid(a));
            anchor.decay_strength = 1.0;
            records.push(anchor);
            let count = if anchors == 11 && targets == 100 && a == 11 {
                1
            } else {
                targets
            };
            for offset in 0..count {
                let t = 1000 + a * 1000 + offset;
                records.push(rec(mid(t)));
                edges.push((a, t, EdgeType::Associative, 1.0));
                edges.push((a, t, EdgeType::Causal, 1.0));
            }
        }
        let mut prior = None;
        for _ in 0..2 {
            edges.reverse();
            records.reverse();
            let (e, _) = fixture(records.clone(), &[], &edges);
            let got = e
                .with_rif_mode(RifMode::Shadow)
                .search_with_semantics(metadata(anchors as usize), history())
                .await
                .unwrap();
            if let Some(old) = &prior {
                assert_eq!(&values(&got), old);
            }
            prior = Some(values(&got));
            let d = got.diagnostics.rif.unwrap();
            assert_eq!(
                d.eligible_pairs,
                if anchors == 11 && targets == 100 {
                    1001
                } else {
                    (anchors * targets) as usize
                }
            );
            assert_eq!(d.admitted_pairs, expected);
            assert_eq!(d.unique_targets, expected);
            assert_eq!(d.per_anchor_cap_hits, anchor_hits);
            assert_eq!(d.pair_cap_hit, pair_hit);
            assert_eq!(d.returned_multipliers.len(), anchors as usize);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_shared_target_uses_one_context_true_stability_and_no_enhancement() {
    for (stability, expected) in [(10.0, 0.75), (0.55, 0.5 / 0.55), (0.4, 1.0)] {
        let mut records: Vec<_> = (1..=20).map(|i| rec(mid(i))).collect();
        let mut target = rec(mid(100));
        target.stability = stability;
        target.decay_strength = 0.8;
        records.push(target);
        let edges: Vec<_> = (1..=20)
            .map(|a| (a, 100, EdgeType::Associative, 1.0))
            .collect();
        let (e, _) = fixture(records, &[], &edges);
        let got = e
            .with_rif_mode(RifMode::Experimental)
            .search_with_semantics(metadata(21), history())
            .await
            .unwrap();
        let d = got.diagnostics.rif.unwrap();
        assert!((d.returned_multipliers[&mid(100)] - expected).abs() < 1e-6);
        assert!(
            d.returned_multipliers
                .values()
                .all(|m| (0.75..=1.0).contains(m))
        );
        let result = got
            .response
            .results
            .iter()
            .find(|r| r.memory_id == mid(100))
            .unwrap();
        assert!((result.effective_r - 0.8 * expected).abs() < 1e-6);
        assert_eq!(result.stability, stability); // Query-local R only, never materialized S.
        if stability == 10.0 {
            assert!(d.cumulative_cap_hits > 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_uses_actual_similarity_and_canonical_retrievability_not_graph_strength() {
    for (cosine, strength, changes) in [(1.0, 0.9, true), (0.0, 0.9, false), (1.0, 0.1, false)] {
        let a = rec(mid(1));
        let mut b = rec(mid(2));
        b.strength = strength;
        let (e, _) = fixture(
            vec![a, b],
            &[(mid(1), 1.0), (mid(2), cosine)],
            &[(1, 2, EdgeType::Associative, 1.0)],
        );
        let got = e
            .with_rif_mode(RifMode::Experimental)
            .search_with_semantics(metadata(2), history())
            .await
            .unwrap();
        let d = got.diagnostics.rif.as_ref().unwrap();
        assert_eq!(d.similarity_available, 2);
        assert_eq!(d.similarity_unavailable, 0);
        assert_eq!(d.returned_multipliers[&mid(2)] < 1.0, changes);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_rejections_never_consume_caps_or_leak_ineligible_ids() {
    let mut records: Vec<_> = (1..=6)
        .map(|i| {
            let mut r = rec(mid(i));
            r.tags = vec![Tag::new("keep").unwrap()];
            r
        })
        .collect();
    records[2].namespace_id = other_ns();
    records[3].phase = DecayPhase::Ghost;
    records[4].phase = DecayPhase::Tombstone;
    records[5].tags.clear();
    let edges: Vec<_> = (2..=6)
        .map(|t| (1, t, EdgeType::Associative, 1.0))
        .collect();
    let (e, _) = fixture(records, &[], &edges);
    let q = query(
        json!({"queryMode":"metadata_only","includeGhosts":true,"filter":{"requireTags":["keep"]},"limit":20,"recordAccess":false}),
    );
    let got = e
        .with_rif_mode(RifMode::Experimental)
        .search_with_semantics(q, history())
        .await
        .unwrap();
    let d = got.diagnostics.rif.unwrap();
    assert_eq!(d.eligible_pairs, 2);
    for n in [3, 5, 6] {
        assert!(!d.returned_multipliers.contains_key(&mid(n)));
    }
    assert_eq!(d.returned_multipliers[&mid(4)], 1.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_graph_errors_are_results_not_silent_empty_success() {
    let (mut e, g) = fixture(
        vec![rec(mid(1)), rec(mid(2))],
        &[],
        &[(1, 2, EdgeType::Associative, 1.0)],
    );
    {
        let mut g = g.write().await;
        let key = g.get_node(&mid(1)).unwrap().outgoing[0];
        g.edges.remove(key);
    }
    assert!(
        e.search_with_semantics(metadata(2), history())
            .await
            .is_ok()
    );
    for mode in [RifMode::Shadow, RifMode::Experimental] {
        e.rif_mode = mode;
        let err = e
            .search_with_semantics(metadata(2), history())
            .await
            .unwrap_err();
        assert!(matches!(err, SearchError::Unavailable(_)), "{err:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_all_channels_keep_baseline_formulas_and_report_unchanged_scores() {
    for channel in [
        RetrievalChannel::Vector,
        RetrievalChannel::Fts,
        RetrievalChannel::Entity,
        RetrievalChannel::Graph,
        RetrievalChannel::Metadata,
    ] {
        let mut records = vec![rec(mid(1)), rec(mid(2))];
        let vectors = match channel {
            RetrievalChannel::Vector => vec![(mid(1), 1.0), (mid(2), 0.8)],
            RetrievalChannel::Entity | RetrievalChannel::Graph => vec![(mid(1), 1.0)],
            _ => vec![],
        };
        if channel == RetrievalChannel::Entity {
            for r in &mut records {
                r.entities = vec!["alice".into()];
            }
        }
        let (mut e, _) = fixture(records, &vectors, &[(1, 2, EdgeType::Associative, 1.0)]);
        let directory = tempfile::tempdir().unwrap();
        let mut q = match channel {
            RetrievalChannel::Metadata => metadata(20),
            RetrievalChannel::Entity => query(json!({"text":"q","entities":["alice"],"limit":20})),
            RetrievalChannel::Graph => query(json!({"text":"q","graphDepth":1,"limit":20})),
            RetrievalChannel::Fts => {
                let fts = crate::search::FtsIndex::new(directory.path()).unwrap();
                for id in [mid(1), mid(2)] {
                    fts.add(ns(), id, "needle", None, &[]).unwrap();
                }
                e.fts_index = Arc::new(SharedFtsIndexRegistry::new(Arc::new(
                    tokio::sync::Mutex::new(fts),
                )));
                query(json!({"text":"needle","limit":20}))
            }
            _ => query(json!({"text":"q","limit":20})),
        };
        q.record_access = false;
        let off = e.search_with_semantics(q.clone(), history()).await.unwrap();
        e.rif_mode = RifMode::Shadow;
        let shadow = e.search_with_semantics(q.clone(), history()).await.unwrap();
        assert_eq!(values(&off), values(&shadow));
        e.rif_mode = RifMode::Experimental;
        let got = e.search_with_semantics(q, history()).await.unwrap();
        let expected_unchanged = matches!(
            channel,
            RetrievalChannel::Vector | RetrievalChannel::Fts | RetrievalChannel::Graph
        );
        assert_eq!(
            hit(&got, 2).composite_score == hit(&off, 2).composite_score,
            expected_unchanged,
            "{channel:?}"
        );
        if expected_unchanged {
            assert!(got.diagnostics.rif.unwrap().unchanged_score_channels[&channel] > 0);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_inheritance_recomputes_each_original_score_and_temporal_boost_once() {
    let mut records: Vec<_> = (1..=3).map(|i| rec(mid(i))).collect();
    records[0].decay_strength = 1.0;
    records[1].decay_strength = 0.2;
    records[2].decay_strength = 0.9;
    let (mut e, _) = fixture(
        records,
        &[],
        &[
            (2, 1, EdgeType::Supersedes, 1.0),
            (3, 1, EdgeType::Associative, 1.0),
        ],
    );
    let mut q = metadata(3);
    q.time_range_start = Some(1_699_999_000_000);
    q.time_range_end = Some(1_700_001_000_000);
    let off = e.search_with_semantics(q.clone(), current()).await.unwrap();
    e.rif_mode = RifMode::Shadow;
    assert_eq!(
        values(&off),
        values(&e.search_with_semantics(q.clone(), current()).await.unwrap())
    );
    e.rif_mode = RifMode::Experimental;
    let got = e.search_with_semantics(q.clone(), current()).await.unwrap();
    let m = crate::rif::plasticity_multiplier(0.3 * 0.9, &crate::rif::RifConfig::default());
    assert!(
        (hit(&got, 2).composite_score.unwrap() - hit(&off, 2).composite_score.unwrap() * m).abs()
            < 1e-6
    );
    assert!(
        (got.diagnostics.provenance.as_ref().unwrap()[&mid(2)]
            .own_score
            .unwrap()
            - 0.03)
            .abs()
            < 1e-6
    );
    assert_eq!(hit(&got, 2).effective_r, 0.2); // Only its predecessor is a target.
    assert!(!got.response.results.iter().any(|r| r.memory_id == mid(1)));
    let history = e.search_with_semantics(q, history()).await.unwrap();
    assert!(
        history
            .response
            .results
            .iter()
            .any(|r| r.memory_id == mid(1))
    );
    assert!(
        history.diagnostics.provenance.unwrap()[&mid(2)]
            .inherited_floor
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_operator_modes_reject_original_write_intent_without_rewriting_it() {
    let (mut e, _) = fixture(vec![rec(mid(1))], &[], &[]);
    let mut q = metadata(1);
    q.record_access = true;
    for mode in [RifMode::Shadow, RifMode::Experimental] {
        e.rif_mode = mode;
        assert!(
            matches!(e.search_with_semantics(q.clone(), history()).await,
            Err(SearchError::InvalidFilter(message)) if message.contains("recordAccess=false"))
        );
        // An operator experiment never changes legacy configuration/defaults.
        assert!(
            e.search_with_semantics(q.clone(), RetrievalSemantics::V1)
                .await
                .is_ok()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_neutral_overlay_keeps_graph_rediscovered_predecessor_and_own_maximum() {
    let mut records: Vec<_> = (1..=3).map(|i| rec(mid(i))).collect();
    // Orthogonal available embeddings make RIF neutral without weakening graph
    // propagation. Graph and metadata contributions must both survive collapse.
    for r in &mut records {
        r.strength = 0.9;
        r.decay_strength = 0.2;
    }
    records[2].decay_strength = 1.0;
    let (mut e, _) = fixture(
        records,
        &[(mid(1), 1.0), (mid(3), 0.0)],
        &[
            (2, 1, EdgeType::Supersedes, 1.0),
            (3, 1, EdgeType::Associative, 1.0),
        ],
    );
    let mut q = metadata(3);
    q.graph_depth = 1;
    let off = e.search_with_semantics(q.clone(), current()).await.unwrap();
    assert!(hit(&off, 2).composite_score.unwrap() > 0.02);
    e.rif_mode = RifMode::Experimental;
    let got = e.search_with_semantics(q, current()).await.unwrap();
    assert_eq!(got.diagnostics.rif.as_ref().unwrap().suppressed_targets, 0);
    assert_eq!(values(&got), values(&off));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_corrupt_weights_and_canonical_stability_fail_without_empty_success() {
    for weight in [f32::NAN, f32::INFINITY, -0.1, 1.1] {
        let (e, _) = fixture(
            vec![rec(mid(1)), rec(mid(2))],
            &[],
            &[(1, 2, EdgeType::Associative, weight)],
        );
        assert!(matches!(
            e.with_rif_mode(RifMode::Shadow)
                .search_with_semantics(metadata(2), history())
                .await,
            Err(SearchError::Unavailable(_))
        ));
    }
    for stability in [f32::NAN, 0.0, -1.0] {
        let mut target = rec(mid(2));
        target.stability = stability;
        let (e, _) = fixture(
            vec![rec(mid(1)), target],
            &[],
            &[(1, 2, EdgeType::Associative, 1.0)],
        );
        assert!(matches!(
            e.with_rif_mode(RifMode::Experimental)
                .search_with_semantics(metadata(2), history())
                .await,
            Err(SearchError::MetadataError(_))
        ));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rif_signed_zero_activation_ties_use_uuid_before_target_caps() {
    for weight in [0.0, -0.0] {
        let mut records = vec![rec(mid(1)), rec(mid(2)), rec(mid(1000))];
        records[0].decay_strength = 1.0;
        records[1].decay_strength = 1.0;
        let mut edges = vec![
            (1, 1000, EdgeType::Associative, weight),
            (2, 1000, EdgeType::Associative, 0.0),
        ];
        for anchor in [1, 2] {
            for offset in 1..=100 {
                let target = anchor * 1000 + offset;
                records.push(rec(mid(target)));
                edges.push((anchor, target, EdgeType::Associative, 0.0));
            }
        }
        let (e, _) = fixture(records, &[], &edges);
        let got = e
            .with_rif_mode(RifMode::Shadow)
            .search_with_semantics(metadata(2), history())
            .await
            .unwrap();
        let d = got.diagnostics.rif.unwrap();
        assert_eq!(d.eligible_pairs, 202);
        assert_eq!(d.admitted_pairs, 200);
        // Shared lowest UUID wins both anchor caps, regardless of the zero sign.
        assert_eq!(d.unique_targets, 199);
        assert_eq!(d.per_anchor_cap_hits, 2);
        assert_eq!(d.suppressed_targets, 0);
    }
}
