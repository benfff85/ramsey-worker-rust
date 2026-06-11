//! Regression suite: work-enumeration safety properties.
//!
//! Workers independently rebuild enumerators from the same stage config and
//! claim disjoint index ranges via Redis counters. Correctness of the whole
//! distributed system therefore rests on two properties locked in here:
//! every index maps to a unique pair (bijectivity), and two independently
//! constructed enumerators agree on the mapping (determinism).

mod common;

use common::{random_bitstring, red_count};
use ramsey_worker_rust::enumeration::{
    create_enumerator, BasicEnumerator, DualCardinalityEnumerator, WorkEnumerator,
};
use ramsey_worker_rust::graph::Graph;
use ramsey_worker_rust::model::WorkEnumerationStrategy;
use std::collections::HashSet;

const N: usize = 20;
const SEED: u64 = 8319;

fn pair_key(e: &ramsey_worker_rust::graph::WorkUnitEdge) -> (u16, u16) {
    if e.vertex_one < e.vertex_two {
        (e.vertex_one, e.vertex_two)
    } else {
        (e.vertex_two, e.vertex_one)
    }
}

/// Independent re-implementation of the cardinality score: count of
/// same-colored edges adjacent to either endpoint (excluding the edge itself).
fn cardinality(graph: &Graph, v1: usize, v2: usize, is_red: bool) -> i32 {
    let mut c = 0;
    for endpoint in [v1, v2] {
        for k in 0..graph.vertex_count {
            if k != v1 && k != v2 && graph.adjacency[endpoint].get(k) == is_red {
                c += 1;
            }
        }
    }
    c
}

#[test]
fn both_enumerators_are_bijective_at_scale() {
    let bits = random_bitstring(SEED, N);
    let g = Graph::from_bitstring(&bits, N);
    let red = red_count(&bits) as i64;
    let blue = (bits.len() - red_count(&bits)) as i64;

    for enumerator in [
        Box::new(BasicEnumerator::new(&g)) as Box<dyn WorkEnumerator>,
        Box::new(DualCardinalityEnumerator::new(&g)) as Box<dyn WorkEnumerator>,
    ] {
        assert_eq!(enumerator.total_pairs(), red * blue);
        let mut seen: HashSet<((u16, u16), (u16, u16))> = HashSet::new();
        for i in 0..enumerator.total_pairs() {
            let (r, b) = enumerator.index_to_edge_pair(i);
            assert!(
                seen.insert((pair_key(&r), pair_key(&b))),
                "duplicate pair at index {i}"
            );
        }
        assert_eq!(seen.len() as i64, red * blue);
    }
}

#[test]
fn enumerators_are_deterministic_across_independent_instances() {
    // Two workers building enumerators from the same stage config MUST map
    // every index to the same pair, or the distributed search silently
    // corrupts. This also guards the stable-sort requirement in the
    // cardinality ordering (ties must break identically every build).
    let bits = random_bitstring(SEED, N);
    let g1 = Graph::from_bitstring(&bits, N);
    let g2 = Graph::from_bitstring(&bits, N);

    let a = DualCardinalityEnumerator::new(&g1);
    let b = DualCardinalityEnumerator::new(&g2);
    assert_eq!(a.total_pairs(), b.total_pairs());
    for i in 0..a.total_pairs() {
        let (ar, ab) = a.index_to_edge_pair(i);
        let (br, bb) = b.index_to_edge_pair(i);
        assert_eq!(pair_key(&ar), pair_key(&br), "red mismatch at index {i}");
        assert_eq!(pair_key(&ab), pair_key(&bb), "blue mismatch at index {i}");
    }

    let a = BasicEnumerator::new(&g1);
    let b = BasicEnumerator::new(&g2);
    for i in 0..a.total_pairs() {
        let (ar, ab) = a.index_to_edge_pair(i);
        let (br, bb) = b.index_to_edge_pair(i);
        assert_eq!(pair_key(&ar), pair_key(&br), "red mismatch at index {i}");
        assert_eq!(pair_key(&ab), pair_key(&bb), "blue mismatch at index {i}");
    }
}

#[test]
fn emitted_pairs_have_correct_colors() {
    // First element of every pair must be a red edge, second blue.
    let bits = random_bitstring(SEED, N);
    let g = Graph::from_bitstring(&bits, N);
    let enumerator = DualCardinalityEnumerator::new(&g);
    for i in 0..enumerator.total_pairs() {
        let (r, b) = enumerator.index_to_edge_pair(i);
        assert!(
            g.adjacency[r.vertex_one as usize].get(r.vertex_two as usize),
            "index {i}: first edge not red"
        );
        assert!(
            !g.adjacency[b.vertex_one as usize].get(b.vertex_two as usize),
            "index {i}: second edge not blue"
        );
    }
}

#[test]
fn dual_cardinality_orders_red_edges_by_descending_cardinality() {
    let bits = random_bitstring(SEED, N);
    let g = Graph::from_bitstring(&bits, N);
    let enumerator = DualCardinalityEnumerator::new(&g);
    let blue_count = (bits.len() - red_count(&bits)) as i64;

    // Walking indices in steps of blue_count yields the red edges in
    // enumeration order; their independently recomputed cardinalities must be
    // non-increasing.
    let mut prev = i32::MAX;
    let mut idx = 0;
    while idx < enumerator.total_pairs() {
        let (r, _) = enumerator.index_to_edge_pair(idx);
        let c = cardinality(&g, r.vertex_one as usize, r.vertex_two as usize, true);
        assert!(
            c <= prev,
            "red cardinality increased: {c} after {prev} at index {idx}"
        );
        prev = c;
        idx += blue_count;
    }
}

#[test]
fn create_enumerator_dispatches_all_strategies_with_consistent_totals() {
    let bits = random_bitstring(SEED, N);
    let g = Graph::from_bitstring(&bits, N);
    let red = red_count(&bits) as i64;
    let blue = (bits.len() - red_count(&bits)) as i64;

    for strategy in [
        WorkEnumerationStrategy::BASIC,
        WorkEnumerationStrategy::SINGLE_EDGE_CARDINALITY,
        WorkEnumerationStrategy::DUAL_EDGE_CARDINALITY,
    ] {
        let enumerator = create_enumerator(&strategy, &g);
        assert_eq!(
            enumerator.total_pairs(),
            red * blue,
            "strategy {strategy:?}"
        );
    }
}
