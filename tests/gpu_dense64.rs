//! Regression and service test for the n=3, 33--64 candidate dense Metal path.
//!
//! This is deliberately an ignored Metal test. It draws real candidate sets from a campaign graph,
//! proves both legacy and dense-64 modes against the CPU count, then alternates their raw GPU runs.
//!
//! DENSE64_REQUIRE_SPEEDUP=1 cargo test --release --test gpu_dense64 -- --ignored --nocapture

#![cfg(target_os = "macos")]

use ramsey_worker_rust::algorithm::count_cliques_through_vertex_set;
use ramsey_worker_rust::gpu::{CorrectionEngine, CorrectionRequest};
use ramsey_worker_rust::graph::Graph;

const FIXTURE: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
const VERTICES: usize = 282;
const CLIQUE_SIZE: usize = 8;
const ORACLE_REQUESTS: usize = 5_000;
const BENCH_REQUESTS: usize = 131_072;

fn graph() -> Graph {
    let bits = FIXTURE.lines().find(|line| !line.trim().is_empty()).unwrap().trim();
    Graph::from_bitstring(bits, VERTICES)
}

fn candidate_cardinality(graph: &Graph, request: &CorrectionRequest) -> usize {
    let adjacency = if request.blue {
        &graph.complement_adjacency
    } else {
        &graph.adjacency
    };
    let mut candidates = adjacency[request.seeds[0] as usize];
    for &seed in request.seeds.iter().take(request.n as usize).skip(1) {
        candidates.and_assign(&adjacency[seed as usize]);
    }
    for &seed in request.seeds.iter().take(request.n as usize) {
        candidates.clear(seed as usize);
    }
    candidates.cardinality() as usize
}

/// Real three-seed neighborhoods from a campaign graph, selected only when they force the current
/// shader's generic fallback but fit the proposed 64-bit dense representation.
fn n3_dense64_requests(graph: &Graph, count: usize) -> Vec<CorrectionRequest> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let mut requests = Vec::with_capacity(count);
    let mut attempts = 0usize;
    while requests.len() < count && attempts < count * 200 {
        attempts += 1;
        let seeds = [
            (next() % VERTICES as u64) as u16,
            (next() % VERTICES as u64) as u16,
            (next() % VERTICES as u64) as u16,
            0,
        ];
        if seeds[0] == seeds[1] || seeds[0] == seeds[2] || seeds[1] == seeds[2] {
            continue;
        }
        let request = CorrectionRequest {
            seeds,
            n: 3,
            blue: next() & 1 == 1,
        };
        if (33..=64).contains(&candidate_cardinality(graph, &request)) {
            requests.push(request);
        }
    }
    assert_eq!(
        requests.len(),
        count,
        "fixture did not yield enough real n=3 33--64-candidate requests"
    );
    requests
}

fn run_rate(engine: &mut CorrectionEngine, requests: &[CorrectionRequest]) -> (Vec<i32>, f64) {
    let started = std::time::Instant::now();
    let results = engine.run(requests).unwrap().to_vec();
    let seconds = started.elapsed().as_secs_f64();
    (results, requests.len() as f64 / seconds / 1e6)
}

#[test]
#[ignore]
fn n3_dense64_is_exact_and_beats_the_legacy_fallback() {
    let graph = graph();
    let oracle_requests = n3_dense64_requests(&graph, ORACLE_REQUESTS);
    let want: Vec<i32> = oracle_requests
        .iter()
        .map(|request| {
            let adjacency = if request.blue {
                &graph.complement_adjacency
            } else {
                &graph.adjacency
            };
            let seeds: Vec<usize> = request.seeds[..request.n as usize]
                .iter()
                .map(|&seed| seed as usize)
                .collect();
            count_cliques_through_vertex_set(adjacency, &seeds, CLIQUE_SIZE)
        })
        .collect();

    let mut legacy = CorrectionEngine::new_with_dense64(&graph, VERTICES, CLIQUE_SIZE, false)
        .expect("Metal device required");
    let mut dense64 = CorrectionEngine::new_with_dense64(&graph, VERTICES, CLIQUE_SIZE, true)
        .expect("Metal device required");
    assert_eq!(legacy.run(&oracle_requests).unwrap(), want, "legacy GPU vs CPU");
    assert_eq!(dense64.run(&oracle_requests).unwrap(), want, "dense-64 GPU vs CPU");

    let benchmark: Vec<CorrectionRequest> = (0..BENCH_REQUESTS)
        .map(|index| oracle_requests[index % oracle_requests.len()])
        .collect();
    let (legacy_a1, legacy_rate_a1) = run_rate(&mut legacy, &benchmark);
    let (dense_a1, dense_rate_a1) = run_rate(&mut dense64, &benchmark);
    let (legacy_a2, legacy_rate_a2) = run_rate(&mut legacy, &benchmark);
    let (dense_a2, dense_rate_a2) = run_rate(&mut dense64, &benchmark);
    assert_eq!(dense_a1, legacy_a1, "first dense-64 run changed a count");
    assert_eq!(dense_a2, legacy_a2, "second dense-64 run changed a count");

    let legacy_rate = (legacy_rate_a1 + legacy_rate_a2) / 2.0;
    let dense_rate = (dense_rate_a1 + dense_rate_a2) / 2.0;
    eprintln!(
        "n=3, |P| 33..64: legacy {:.2} M/s ({legacy_rate_a1:.2}, {legacy_rate_a2:.2}); \
         dense-64 {:.2} M/s ({dense_rate_a1:.2}, {dense_rate_a2:.2}); {:.2}x",
        legacy_rate,
        dense_rate,
        dense_rate / legacy_rate,
    );
    if std::env::var_os("DENSE64_REQUIRE_SPEEDUP").is_some() {
        assert!(
            dense_rate > legacy_rate * 1.05,
            "dense-64 must clear a 5% raw-service improvement before it is considered for fleet A/B"
        );
    }
}
