//! Exhaustive and production-scale checks for the exact separable row selector.

use ramsey_worker_rust::algorithm::get_all_cliques;
use ramsey_worker_rust::clique_collection::CliqueCollection;
use ramsey_worker_rust::enumeration::{SequentialWithSinglesEnumerator, WorkEnumerator, WorkUnit};
use ramsey_worker_rust::graph::{Graph, WorkUnitEdge};
use ramsey_worker_rust::hoist::{cross_pairs, CrossPairs, HoistTables, PairOutcome};
use ramsey_worker_rust::separable::{RowSelectorScratch, SeparableRowPlan};

fn collection_for(graph: &mut Graph, k: usize) -> CliqueCollection {
    let mut collection = CliqueCollection::new(graph.vertex_count);
    collection.set_cliques(get_all_cliques(graph, k), graph.vertex_count);
    collection
}

#[test]
fn selector_matches_the_legacy_pre_correction_decision_on_every_pair() {
    // Deliberately non-symmetric 8-vertex graph so both colours and all cross-pair categories
    // occur. Every red/blue pair is checked, including shared vertices.
    let bits = "1101011010101011001100101010";
    let mut graph = Graph::from_bitstring(bits, 8);
    let collection = collection_for(&mut graph, 4);
    let mut tables = HoistTables::new(8);
    tables.fill_slice(&mut graph, 4, 0, 1);
    let plan = SeparableRowPlan::build(&mut graph, 4, &collection, &mut tables);
    let mut scratch = RowSelectorScratch::new(plan.blue_len());

    for threshold in [-10, -1, 0, 1, 5, 20, 100] {
        for red_index in 0..plan.red_len() {
            let selected = plan.select_row(&graph, red_index, threshold, &mut scratch);
            for blue_index in 0..plan.blue_len() {
                let r = plan.red_edge(red_index);
                let b = plan.blue_edge(blue_index);
                let broken = collection.get_count_of_cliques_containing_edges(&[
                    WorkUnitEdge {
                        vertex_one: r.0 as u16,
                        vertex_two: r.1 as u16,
                    },
                    WorkUnitEdge {
                        vertex_one: b.0 as u16,
                        vertex_two: b.1 as u16,
                    },
                ]);
                let early_limit = (threshold - 1) - collection.total() as i32 + broken;
                // This is exactly the pre-correction branch the normal worker would take. A
                // `NeedsCorrection` may still reject after its exact count, but it must not be
                // retired; an early negative limit is skipped before `pair_classify` is called.
                let must_evaluate = early_limit >= 0
                    && !matches!(
                        tables.pair_classify(&mut graph, 4, r, b, early_limit),
                        PairOutcome::Rejected
                    );
                assert_eq!(
                    selected.contains(&blue_index),
                    must_evaluate,
                    "threshold={threshold}, r={r:?}, b={b:?}"
                );
                if !selected.contains(&blue_index) {
                    // Independent outcome oracle for every retirement on this exhaustive small
                    // graph: do the unbounded correction, not the selector's lower-bound logic.
                    let count = collection.total() as i32 - broken
                        + tables.pair_created(&mut graph, 4, r, b);
                    assert!(
                        count >= threshold,
                        "selector retired a real candidate: threshold={threshold}, r={r:?}, b={b:?}, count={count}"
                    );
                }
            }
        }
    }
}

#[test]
fn complete_rows_match_sequential_indices_and_fallback_ranges_cover_exactly_once() {
    let bits = "1101011010101011001100101010";
    let mut graph = Graph::from_bitstring(bits, 8);
    let collection = collection_for(&mut graph, 4);
    let mut tables = HoistTables::new(8);
    tables.fill_slice(&mut graph, 4, 0, 1);
    let plan = SeparableRowPlan::build(&mut graph, 4, &collection, &mut tables);
    let enumerator = SequentialWithSinglesEnumerator::new(&graph);
    let total = enumerator.total_work_units();

    // The plan's red/blue order is the exact pair order after the singles prefix.
    for red_index in 0..plan.red_len() {
        for blue_index in 0..plan.blue_len() {
            let absolute = plan.singles_len()
                + (red_index * plan.blue_len() + blue_index) as i64;
            assert_eq!(
                enumerator.index_to_work_unit(absolute),
                WorkUnit::PairFlip(
                    WorkUnitEdge {
                        vertex_one: plan.red_edge(red_index).0 as u16,
                        vertex_two: plan.red_edge(red_index).1 as u16,
                    },
                    WorkUnitEdge {
                        vertex_one: plan.blue_edge(blue_index).0 as u16,
                        vertex_two: plan.blue_edge(blue_index).1 as u16,
                    },
                ),
                "red row {red_index}, blue offset {blue_index}"
            );
        }
    }

    // For every possible Redis-claimed subrange, row skipping plus ordinary one-unit fallback
    // covers exactly the original number of logical units. This is the processed_count contract.
    for start in 0..total {
        for end in (start + 1)..=total {
            let mut index = start;
            let mut accounted = 0i64;
            while index < end {
                if plan.complete_row_starting_at(index, end).is_some() {
                    index += plan.blue_len() as i64;
                    accounted += plan.blue_len() as i64;
                } else {
                    index += 1;
                    accounted += 1;
                }
            }
            assert_eq!(accounted, end - start, "range [{start}, {end})");
        }
    }
}

/// Production-sized oracle: every pair the selector retires — including all-red/all-blue pairs
/// retired by the existing lower bounds — must be rejected by the unchanged bounded evaluator,
/// not merely by another implementation of the selector arithmetic.
#[test]
#[ignore]
fn selector_retirements_match_the_legacy_bounded_evaluator_on_a_live_graph() {
    const V: usize = 282;
    const K: usize = 8;
    const FALLBACK: &str = include_str!("fixtures/campaign3-consecutive-bases.txt");
    let bits = std::env::var("LIVE_GRAPH")
        .ok()
        .filter(|value| value.trim().len() == V * (V - 1) / 2)
        .unwrap_or_else(|| FALLBACK.lines().next().unwrap().trim().to_string());
    let threshold: i32 = std::env::var("LIVE_THRESHOLD")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(743_716);
    let mut graph = Graph::from_bitstring(bits.trim(), V);
    let collection = collection_for(&mut graph, K);
    let base_total = collection.total() as i32;
    let mut tables = HoistTables::new(V);
    tables.fill_slice(&mut graph, K, 0, 1);
    let plan = SeparableRowPlan::build(&mut graph, K, &collection, &mut tables);
    let mut scratch = RowSelectorScratch::new(plan.blue_len());
    let mut retired = 0usize;
    let mut retired_mixed = 0usize;
    let mut retired_slow = 0usize;

    let broken = |edge: (usize, usize)| {
        collection.get_count_of_cliques_containing_edges(&[WorkUnitEdge {
            vertex_one: edge.0 as u16,
            vertex_two: edge.1 as u16,
        }])
    };
    for red_index in 0..plan.red_len() {
        plan.select_row(&graph, red_index, threshold, &mut scratch);
        let r = plan.red_edge(red_index);
        for blue_index in 0..plan.blue_len() {
            if scratch.contains(blue_index) {
                continue;
            }
            retired += 1;
            let b = plan.blue_edge(blue_index);
            let limit = (threshold - 1) - base_total + broken(r) + broken(b);
            match cross_pairs(&graph.adjacency, r, b) {
                CrossPairs::Mixed => retired_mixed += 1,
                CrossPairs::AllRed | CrossPairs::AllBlue => retired_slow += 1,
            }
            assert_eq!(
                tables.pair_created_bounded(&mut graph, K, r, b, limit),
                None,
                "legacy bounded evaluator accepted selector retirement r={r:?}, b={b:?}"
            );
        }
    }
    eprintln!(
        "legacy oracle rejected all {retired} selector-retired pairs at threshold {threshold} ({retired_mixed} Mixed, {retired_slow} slow)"
    );
}
