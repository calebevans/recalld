use recalld::model::{MemoryId, NamespaceId};
use recalld::search::{FlatVectorIndex, SearchFilter, VectorIndex, VectorMetadata, normalize_l2};
use std::{hint::black_box, time::Instant};

fn vector(state: &mut u64) -> Vec<f32> {
    let mut v: Vec<f32> = (0..384).map(|_| {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state >> 40) as f32 / 8_388_608.0 - 1.0
    }).collect();
    normalize_l2(&mut v);
    v
}

fn main() {
    for count in [1_000, 10_000] {
        let mut state = 42_u64;
        let mut index = FlatVectorIndex::new(384);
        index.reserve(count);
        for i in 0..count {
            index.add(
                MemoryId::from_bytes((i as u128 + 1).to_be_bytes()),
                &vector(&mut state),
                VectorMetadata {
                    namespace_id: NamespaceId::new(if i % 10 == 0 { 1 } else { 2 }),
                    decay_phase: 1,
                    tags: Vec::new(),
                },
            ).unwrap();
        }
        let mut query_state = 43_u64;
        let queries: Vec<_> = (0..100).map(|_| vector(&mut query_state)).collect();
        for filtered in [false, true] {
            let filter = SearchFilter {
                namespace_id: filtered.then(|| NamespaceId::new(1)),
                ..Default::default()
            };
            for query in queries.iter().take(20) {
                black_box(index.search(black_box(query), 10, black_box(&filter)).unwrap());
            }
            let mut samples = Vec::with_capacity(100);
            for query in &queries {
                let start = Instant::now();
                let result = black_box(index.search(black_box(query), 10, black_box(&filter)).unwrap());
                let nanos = start.elapsed().as_nanos() as u64;
                assert_eq!(result.len(), 10);
                samples.push(nanos);
            }
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            println!("{}", serde_json::json!({
                "vectors": count, "dimension": 384, "k": 10,
                "filtered": filtered, "eligible_fraction": if filtered { 0.1 } else { 1.0 },
                "warmup": 20, "queries": 100,
                "p50_us": sorted[49] as f64 / 1000.0,
                "p95_us": sorted[94] as f64 / 1000.0,
                "samples_ns": samples,
            }));
        }
    }
}
