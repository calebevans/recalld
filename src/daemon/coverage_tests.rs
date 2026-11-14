//! Real scan -> daemon framing -> remote adapter -> MCP coverage regressions.
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::net::UnixListener;

use super::*;
use crate::daemon::bridge_adapters::RemoteSearchAdapter;
use crate::mcp::bridge::{DUPLICATE_NEIGHBOR_CAP, DUPLICATE_SAMPLE_CAP};
use crate::search::{VectorIndex, VectorMetadata};
use crate::test_support::{Fixture, MemorySpec, StubBridge};

async fn indexed_memory(f: &Fixture, phase: crate::model::DecayPhase) -> MemoryId {
    let embedding = [1.0, 0.0, 0.0, 0.0];
    let id = f
        .insert_memory_with(MemorySpec {
            phase,
            embedding: Some(&embedding),
            ..MemorySpec::new(crate::model::NamespaceId::new(1))
        })
        .await;
    f.vector_index
        .write()
        .await
        .add(
            id,
            &embedding,
            VectorMetadata {
                namespace_id: crate::model::NamespaceId::new(1),
                decay_phase: phase.as_u8(),
                tags: vec![],
            },
        )
        .unwrap();
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scan_coverage_counts_live_sample_missing_vectors_and_dense_truncation() {
    use crate::model::DecayPhase;
    let f = Fixture::new();
    let search = f.search_adapter();
    let empty = search
        .scan_duplicates_with_coverage("default", 0.85, DUPLICATE_SAMPLE_CAP)
        .await
        .unwrap();
    let coverage = empty.coverage.unwrap();
    assert_eq!(
        (
            coverage.eligible_count,
            coverage.sampled_count,
            coverage.skipped_vector_count
        ),
        (0, 0, 0)
    );
    assert!(!coverage.complete && !coverage.truncated);

    let missing = f.insert_memory().await;
    let single = search
        .scan_duplicates_with_coverage("default", 0.85, DUPLICATE_SAMPLE_CAP)
        .await
        .unwrap();
    let coverage = single.coverage.unwrap();
    assert_eq!(
        (
            coverage.eligible_count,
            coverage.sampled_count,
            coverage.skipped_vector_count
        ),
        (1, 1, 1)
    );
    assert!(!coverage.complete);

    let ghost = indexed_memory(&f, DecayPhase::Ghost).await;
    let deleted = indexed_memory(&f, DecayPhase::Full).await;
    f.tombstone(deleted); // Deliberately leave a stale vector: it must not become a sampled record.
    let foreign = f.create_namespace("other");
    f.insert_memory_in(foreign, &[]).await;
    let before = format!(
        "{:?}{:?}{:?}",
        f.record(missing),
        f.record(ghost),
        f.record(deleted)
    );
    let report = search
        .scan_duplicates_with_coverage("default", 0.85, DUPLICATE_SAMPLE_CAP)
        .await
        .unwrap();
    let coverage = report.coverage.unwrap();
    assert_eq!(
        (
            coverage.eligible_count,
            coverage.sampled_count,
            coverage.skipped_vector_count
        ),
        (2, 2, 1)
    );
    assert!(!coverage.complete && !coverage.truncated);
    assert_eq!(
        before,
        format!(
            "{:?}{:?}{:?}",
            f.record(missing),
            f.record(ghost),
            f.record(deleted)
        )
    );

    // More than 200 records and more than 11 identical neighbors: neither
    // a full sample nor a dense cluster establishes exhaustive pair coverage.
    for _ in 0..201 {
        indexed_memory(&f, DecayPhase::Full).await;
    }
    let report = search
        .scan_duplicates_with_coverage("default", 0.85, DUPLICATE_SAMPLE_CAP)
        .await
        .unwrap();
    let coverage = report.coverage.unwrap();
    assert_eq!(
        (coverage.eligible_count, coverage.sampled_count),
        (203, 200)
    );
    assert_eq!(coverage.neighbor_cap, DUPLICATE_NEIGHBOR_CAP);
    assert!(coverage.truncated && coverage.counts_available);
    assert!(!coverage.complete);
    assert!(coverage.skipped_vector_count <= 1);
    assert!(!report.clusters.is_empty());
    assert!(
        report
            .clusters
            .iter()
            .flat_map(|c| &c.memories)
            .all(|m| m.id != deleted.to_string())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_scan_coverage_round_trip_and_old_peer_compatibility() {
    for legacy_daemon in [false, true] {
        let f = Arc::new(Fixture::new());
        indexed_memory(&f, crate::model::DecayPhase::Full).await;
        indexed_memory(&f, crate::model::DecayPhase::Full).await;
        f.insert_memory().await;
        let search = f.search_adapter();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coverage.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let unused = StubBridge::default();
            for _ in 0..3 {
                let request = read_framed_message(&mut socket, MAX_MESSAGE_SIZE)
                    .await
                    .unwrap();
                let mut params = request.params;
                if legacy_daemon {
                    params.as_object_mut().unwrap().remove("includeCoverage");
                }
                let result = dispatch(&search, &unused, &unused, &unused, &request.method, params)
                    .await
                    .unwrap();
                write_framed_message(
                    &mut socket,
                    &DaemonResponse::success(request.id, result),
                    MAX_MESSAGE_SIZE,
                )
                .await
                .unwrap();
            }
        });
        let client = Arc::new(crate::daemon::DaemonClient::connect(&path).await.unwrap());
        let remote = RemoteSearchAdapter::new(client);
        let report = remote
            .scan_duplicates_with_coverage("default", 0.85, 200)
            .await
            .unwrap();
        assert_eq!(report.clusters.len(), 1);
        if legacy_daemon {
            assert!(report.coverage.is_none());
        } else {
            let coverage = report.coverage.unwrap();
            assert_eq!(
                (
                    coverage.eligible_count,
                    coverage.sampled_count,
                    coverage.skipped_vector_count
                ),
                (3, 3, 1)
            );
            assert!(!coverage.complete && !coverage.truncated);
        }
        // A caller that does not request coverage still receives the old array.
        assert_eq!(
            remote
                .scan_duplicates("default", 0.85, 200)
                .await
                .unwrap()
                .len(),
            1
        );
        let mut bridge = StubBridge::default().into_bridge();
        bridge.search = Arc::new(remote);
        let result = crate::mcp::tools::dispatch_tool(
            &bridge,
            "find_similar_memories",
            json!({"mode": "scan"}),
        )
        .await;
        assert_ne!(result.is_error, Some(true));
        let wire = serde_json::to_value(result).unwrap();
        let result: Value =
            serde_json::from_str(wire["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(result["clusterCount"], 1);
        assert_eq!(result["coverage"]["countsAvailable"], !legacy_daemon);
        assert_eq!(result["coverage"]["complete"], false);
        if legacy_daemon {
            for field in [
                "sampledCount",
                "eligibleCount",
                "skippedVectorCount",
                "truncated",
            ] {
                assert!(result["coverage"][field].is_null());
            }
        } else {
            assert_eq!(result["coverage"]["sampledCount"], 3);
            assert_eq!(result["coverage"]["eligibleCount"], 3);
            assert_eq!(result["coverage"]["skippedVectorCount"], 1);
            assert_eq!(result["coverage"]["truncated"], false);
        }
        server.await.unwrap();
    }
}
