//! Regression suite: end-to-end invariants of the SA, VDS, and tabu run
//! functions, wired the same way worker.rs wires them (graph + CliqueCollection
//! built from a comprehensive enumeration).
//!
//! These modes are retired in production but kept buildable for future
//! re-runs; this suite keeps their contracts honest: reported counts must
//! match independent recounts, balanced moves must preserve the red-edge
//! count, and seeded runs must be reproducible.

mod common;

use common::{random_bitstring, red_count};
use ramsey_worker_rust::algorithm::{get_all_cliques, get_cliques_comprehensive};
use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::graph::Graph;
use ramsey_worker_rust::sa::{run_sa, SaConfig};
use ramsey_worker_rust::tabu::{run_tabu, TabuConfig};
use ramsey_worker_rust::vds::{run_vds, VdsConfig};

const N: usize = 14;
const K: usize = 4;
const SEED_BITS: u64 = 8319;

fn fixture() -> (Graph, CliqueCollection, i32, usize) {
    let bits = random_bitstring(SEED_BITS, N);
    let mut graph = Graph::from_bitstring(&bits, N);
    let cliques = get_all_cliques(&mut graph, K);
    let base_count = cliques.len() as i32;
    let mut cc = CliqueCollection::new(N);
    cc.set_cliques(cliques, N);
    (graph, cc, base_count, red_count(&bits))
}

fn tabu_config(seed: u64) -> TabuConfig {
    TabuConfig {
        max_iterations: 200,
        base_tabu_tenure: 8,
        max_tabu_tenure: 16,
        restart_after: 50,
        candidate_pool_size: 5,
        diversification_pair_count: 3,
        random_seed: Some(seed),
    }
}

#[test]
fn tabu_preserves_balance_and_reports_recount_consistent_best() {
    let (graph, cc, base_count, base_red) = fixture();
    let result = run_tabu(&graph, K, &tabu_config(7), &cc, None);

    assert_eq!(result.initial_clique_count, base_count);
    assert!(result.iterations_completed <= 200);

    // Balanced pair moves (and balanced diversification) never change the
    // red-edge count.
    let best_bits = &result.best_graph_bitstring;
    assert_eq!(best_bits.len(), N * (N - 1) / 2);
    assert_eq!(red_count(best_bits), base_red, "balance invariant violated");

    // The reported best count must match an independent full recount of the
    // reported best graph.
    let mut best_graph = Graph::from_bitstring(best_bits, N);
    assert_eq!(
        get_cliques_comprehensive(&mut best_graph, K),
        result.best_clique_count,
        "reported best count disagrees with full recount"
    );

    // improved implies strictly better than the starting point.
    if result.improved {
        assert!(result.best_clique_count < base_count);
    }
}

#[test]
fn tabu_is_deterministic_with_a_fixed_seed() {
    let (graph, cc, _, _) = fixture();
    let a = run_tabu(&graph, K, &tabu_config(42), &cc, None);
    let b = run_tabu(&graph, K, &tabu_config(42), &cc, None);
    assert_eq!(a.best_graph_bitstring, b.best_graph_bitstring);
    assert_eq!(a.best_clique_count, b.best_clique_count);
    assert_eq!(a.iterations_completed, b.iterations_completed);
    assert_eq!(a.improvements_found, b.improvements_found);
    assert_eq!(a.diversifications_triggered, b.diversifications_triggered);
}

#[test]
fn vds_reported_result_matches_applying_its_flips() {
    let (graph, cc, base_count, _) = fixture();
    let config = VdsConfig {
        max_depth: 5,
        top_first_edges: 10,
        branching_factor: 4,
        worsening_tolerance: 50,
        random_seed: Some(11),
        start_depth: 3,
    };
    let result = run_vds(&graph, K, &config, &cc, base_count);

    if result.improved {
        assert!(result.final_clique_count < base_count);
        assert!(!result.edges_to_flip.is_empty());
        // Applying the reported flip sequence to the base graph must produce
        // exactly the reported count.
        let mut mutated = Graph::from_bitstring(&graph.to_bitstring(), N);
        mutated.flip_edges(&result.edges_to_flip);
        assert_eq!(
            get_cliques_comprehensive(&mut mutated, K),
            result.final_clique_count,
            "VDS flip list does not reproduce its reported count"
        );
    }
}

#[test]
fn vds_is_deterministic_with_a_fixed_seed() {
    let (graph, cc, base_count, _) = fixture();
    let config = VdsConfig {
        max_depth: 5,
        top_first_edges: 10,
        branching_factor: 4,
        worsening_tolerance: 50,
        random_seed: Some(99),
        start_depth: 3,
    };
    let a = run_vds(&graph, K, &config, &cc, base_count);
    let b = run_vds(&graph, K, &config, &cc, base_count);
    assert_eq!(a.improved, b.improved);
    assert_eq!(a.final_clique_count, b.final_clique_count);
    let edges = |r: &ramsey_worker_rust::vds::VdsRunResult| -> Vec<(u16, u16)> {
        r.edges_to_flip
            .iter()
            .map(|e| (e.vertex_one, e.vertex_two))
            .collect()
    };
    assert_eq!(edges(&a), edges(&b));
}

#[test]
fn sa_reports_recount_consistent_best_and_preserves_balance() {
    // SaConfig has no seed knob, so assert only trajectory-independent
    // invariants: bitstring validity, balance preservation (SA moves are
    // red+blue pairs), and recount consistency of the reported best.
    let (graph, cc, base_count, base_red) = fixture();
    let config = SaConfig {
        max_iterations: 300,
        initial_temp: 10.0,
        cooling_rate: 0.99,
        min_pairs: 2,
        max_pairs: 3,
    };
    let result = run_sa(&graph, K, &config, &cc, None);

    let best_bits = &result.best_graph_bitstring;
    assert_eq!(best_bits.len(), N * (N - 1) / 2);
    assert_eq!(red_count(best_bits), base_red, "balance invariant violated");

    let mut best_graph = Graph::from_bitstring(best_bits, N);
    assert_eq!(
        get_cliques_comprehensive(&mut best_graph, K),
        result.best_clique_count,
        "reported best count disagrees with full recount"
    );

    if result.improved {
        assert!(result.best_clique_count < base_count);
    }
}
