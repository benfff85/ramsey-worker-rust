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
    BasicEnumerator, DualCardinalityEnumerator, DualCardinalityWithSinglesEnumerator,
    WorkEnumerator, WorkUnit, create_enumerator,
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
        assert_eq!(enumerator.total_work_units(), red * blue);
        let mut seen: HashSet<((u16, u16), (u16, u16))> = HashSet::new();
        for i in 0..enumerator.total_work_units() {
            let WorkUnit::PairFlip(r, b) = enumerator.index_to_work_unit(i) else {
                panic!("expected pair at index {i}");
            };
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
    assert_eq!(a.total_work_units(), b.total_work_units());
    for i in 0..a.total_work_units() {
        let WorkUnit::PairFlip(ar, ab) = a.index_to_work_unit(i) else {
            panic!("expected pair at index {i}");
        };
        let WorkUnit::PairFlip(br, bb) = b.index_to_work_unit(i) else {
            panic!("expected pair at index {i}");
        };
        assert_eq!(pair_key(&ar), pair_key(&br), "red mismatch at index {i}");
        assert_eq!(pair_key(&ab), pair_key(&bb), "blue mismatch at index {i}");
    }

    let a = BasicEnumerator::new(&g1);
    let b = BasicEnumerator::new(&g2);
    for i in 0..a.total_work_units() {
        let WorkUnit::PairFlip(ar, ab) = a.index_to_work_unit(i) else {
            panic!("expected pair at index {i}");
        };
        let WorkUnit::PairFlip(br, bb) = b.index_to_work_unit(i) else {
            panic!("expected pair at index {i}");
        };
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
    for i in 0..enumerator.total_work_units() {
        let WorkUnit::PairFlip(r, b) = enumerator.index_to_work_unit(i) else {
            panic!("expected pair at index {i}");
        };
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
    while idx < enumerator.total_work_units() {
        let WorkUnit::PairFlip(r, _) = enumerator.index_to_work_unit(idx) else {
            panic!("expected pair at index {idx}");
        };
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
            enumerator.total_work_units(),
            red * blue,
            "strategy {strategy:?}"
        );
    }

    let singles = red + blue;
    let hybrid = create_enumerator(
        &WorkEnumerationStrategy::DUAL_EDGE_CARDINALITY_WITH_SINGLES,
        &g,
    );
    assert_eq!(hybrid.total_work_units(), singles + red * blue);
}

#[test]
fn hybrid_enumerator_is_bijective_and_deterministic_at_scale() {
    let bits = random_bitstring(SEED, N);
    let g1 = Graph::from_bitstring(&bits, N);
    let g2 = Graph::from_bitstring(&bits, N);
    let a = DualCardinalityWithSinglesEnumerator::new(&g1);
    let b = DualCardinalityWithSinglesEnumerator::new(&g2);

    let red = red_count(&bits) as i64;
    let blue = (bits.len() - red_count(&bits)) as i64;
    let singles = red + blue;
    assert_eq!(a.total_work_units(), singles + red * blue);

    // Bijectivity over the full index space, treating singles and pairs as
    // distinct key spaces; plus instance determinism at every index.
    let mut seen_singles: HashSet<(u16, u16)> = HashSet::new();
    let mut seen_pairs: HashSet<((u16, u16), (u16, u16))> = HashSet::new();
    for i in 0..a.total_work_units() {
        let ua = a.index_to_work_unit(i);
        let ub = b.index_to_work_unit(i);
        assert_eq!(ua, ub, "instances diverge at index {i}");
        match ua {
            WorkUnit::SingleFlip(e) => {
                assert!(i < singles, "single appeared in pair region at {i}");
                assert!(seen_singles.insert(pair_key(&e)), "dup single at {i}");
            }
            WorkUnit::PairFlip(r, bl) => {
                assert!(i >= singles, "pair appeared in singles region at {i}");
                assert!(
                    seen_pairs.insert((pair_key(&r), pair_key(&bl))),
                    "dup pair at {i}"
                );
            }
        }
    }
    assert_eq!(seen_singles.len() as i64, singles);
    assert_eq!(seen_pairs.len() as i64, red * blue);
}
