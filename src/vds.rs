use std::collections::HashSet;

use crate::algorithm::get_new_cliques_with_limit;
use crate::clique_collection::CliqueCollection;
use crate::graph::{Graph, WorkUnitEdge};
use crate::log_info;

struct SearchState {
    best_sequence: Vec<WorkUnitEdge>,
    best_count: i32,
}

pub struct VdsConfig {
    pub max_depth: usize,
    pub top_first_edges: usize,
    pub branching_factor: usize,
    pub worsening_tolerance: i32,
    pub random_seed: Option<u64>,
}

pub struct VdsRunResult {
    pub edges_to_flip: Vec<WorkUnitEdge>,
    pub final_clique_count: i32,
    pub improved: bool,
}

/// Compute the clique count that would result from flipping `edge`, without
/// permanently modifying the graph. Uses the incremental formula:
///   new_total = base_total - broken_cliques + new_cliques
/// where `broken_cliques` is read from the (pristine) CliqueCollection and
/// `new_cliques` is computed via seeded Bron-Kerbosch on the flipped graph.
///
/// This is the correctness oracle for depth-1 VDS. At depth >= 2 the caller
/// must track `destroyed_cliques` to adjust `broken_cliques` for prior flips.
pub fn evaluate_flip(
    graph: &mut Graph,
    clique_collection: &CliqueCollection,
    edge: &WorkUnitEdge,
    clique_size: usize,
    base_total: i32,
) -> i32 {
    let broken = clique_collection.get_count_of_cliques_containing_edges(&[edge.clone()]);
    graph.flip_edges(&[edge.clone()]);
    let (new, _) = get_new_cliques_with_limit(graph, clique_size, &[edge.clone()], i32::MAX);
    graph.flip_edges(&[edge.clone()]); // unflip (restore)
    base_total - broken + new
}

/// Return the top-K edges (across both colors) ranked by the number of cliques
/// they participate in, descending. Ties broken by stable vertex-pair order.
pub fn rank_edges_by_participation(
    graph: &Graph,
    clique_collection: &CliqueCollection,
    top_k: usize,
) -> Vec<WorkUnitEdge> {
    let v = graph.vertex_count;
    let mut scored: Vec<(i32, WorkUnitEdge)> = Vec::new();

    for u in 0..v {
        for w in (u + 1)..v {
            let edge = WorkUnitEdge {
                vertex_one: u as u16,
                vertex_two: w as u16,
            };
            let score = clique_collection.get_count_of_cliques_containing_edges(&[edge.clone()]);
            scored.push((score, edge));
        }
    }

    // Sort descending by score, then by vertex pair for stability
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.vertex_one.cmp(&b.1.vertex_one))
            .then(a.1.vertex_two.cmp(&b.1.vertex_two))
    });

    scored.into_iter().take(top_k).map(|(_, e)| e).collect()
}

/// Run one complete variable-depth search starting from `base_graph`.
///
/// Performs Lin-Kernighan style tree search: at each recursion level, tries the
/// top-K candidate edge flips and records the best cumulative delta found.
/// Returns the best improving edge sequence (possibly empty if no improvement).
///
/// `clique_collection` must be built from `base_graph`.
pub fn run_vds(
    base_graph: &Graph,
    clique_size: usize,
    config: &VdsConfig,
    clique_collection: &CliqueCollection,
    base_clique_count: i32,
) -> VdsRunResult {
    log_info!(
        "VDS starting: vertex_count={}, clique_size={}, base_cliques={}, \
         max_depth={}, top_first_edges={}, branching_factor={}, worsening_tolerance={}",
        base_graph.vertex_count, clique_size, base_clique_count,
        config.max_depth, config.top_first_edges, config.branching_factor, config.worsening_tolerance
    );

    let mut working_graph = Graph::from_bitstring(&base_graph.to_bitstring(), base_graph.vertex_count);
    let candidates = rank_edges_by_participation(&working_graph, clique_collection, config.top_first_edges);

    let mut state = SearchState {
        best_sequence: Vec::new(),
        best_count: base_clique_count,
    };
    let mut current_sequence: Vec<WorkUnitEdge> = Vec::new();
    let mut destroyed: HashSet<u32> = HashSet::new();

    for first_edge in &candidates {
        // Compute delta for this first flip
        let delta = compute_delta(&mut working_graph, clique_collection, first_edge, clique_size, &destroyed);

        // Apply the flip and update destroyed set
        let destroyed_now: Vec<u32> = clique_collection
            .get_cliques_containing_edge(first_edge)
            .iter()
            .filter(|c| !destroyed.contains(c))
            .copied()
            .collect();
        working_graph.flip_edges(&[first_edge.clone()]);
        for c in &destroyed_now {
            destroyed.insert(*c);
        }
        current_sequence.push(first_edge.clone());

        let cumulative = base_clique_count + delta;
        if cumulative < state.best_count {
            state.best_count = cumulative;
            state.best_sequence = current_sequence.clone();
        }

        // Recurse into deeper levels
        if config.max_depth >= 2 {
            search_depth(
                &mut working_graph, clique_collection, clique_size,
                cumulative, 2, config.max_depth,
                &mut destroyed, &mut current_sequence,
                &mut state, config, base_clique_count,
            );
        }

        // Backtrack: unflip, restore destroyed set
        working_graph.flip_edges(&[first_edge.clone()]);
        for c in &destroyed_now {
            destroyed.remove(c);
        }
        current_sequence.pop();
    }

    let improved = !state.best_sequence.is_empty() && state.best_count < base_clique_count;

    // Final verification: apply the best sequence and recount comprehensively.
    // If the running-delta was optimistic (tracker bug), bail out.
    if improved {
        let mut verify_graph = Graph::from_bitstring(&base_graph.to_bitstring(), base_graph.vertex_count);
        verify_graph.flip_edges(&state.best_sequence);
        let verified_count = crate::algorithm::get_cliques_comprehensive(&mut verify_graph, clique_size);
        if verified_count != state.best_count {
            log_info!(
                "VDS verification MISMATCH: tracker said {}, comprehensive says {} — skipping submission",
                state.best_count, verified_count
            );
            return VdsRunResult {
                edges_to_flip: Vec::new(),
                final_clique_count: base_clique_count,
                improved: false,
            };
        }
        log_info!(
            "VDS verified: base={}, final={}, sequence_len={}",
            base_clique_count, verified_count, state.best_sequence.len()
        );
    } else {
        log_info!("VDS finished: no improvement found");
    }

    VdsRunResult {
        edges_to_flip: state.best_sequence,
        final_clique_count: state.best_count,
        improved,
    }
}

fn compute_delta(
    working_graph: &mut Graph,
    clique_collection: &CliqueCollection,
    edge: &WorkUnitEdge,
    clique_size: usize,
    destroyed: &HashSet<u32>,
) -> i32 {
    // Broken = cliques in CC containing this edge MINUS those already destroyed
    let broken: i32 = clique_collection
        .get_cliques_containing_edge(edge)
        .iter()
        .filter(|c| !destroyed.contains(c))
        .count() as i32;

    working_graph.flip_edges(&[edge.clone()]);
    let (new, _) = get_new_cliques_with_limit(working_graph, clique_size, &[edge.clone()], i32::MAX);
    working_graph.flip_edges(&[edge.clone()]); // restore
    -broken + new
}

fn search_depth(
    working_graph: &mut Graph,
    clique_collection: &CliqueCollection,
    clique_size: usize,
    cumulative_count: i32,
    current_depth: usize,
    max_depth: usize,
    destroyed: &mut HashSet<u32>,
    current_sequence: &mut Vec<WorkUnitEdge>,
    state: &mut SearchState,
    config: &VdsConfig,
    base_clique_count: i32,
) {
    // Generate and rank candidates for this level (top branching_factor).
    // Rank from the ORIGINAL clique_collection (fast); could be refined per-depth.
    let candidates = rank_edges_by_participation(working_graph, clique_collection, config.branching_factor);

    for edge in &candidates {
        // Skip edges already in the sequence (prevents same-edge-twice no-op)
        if current_sequence.iter().any(|e| e.vertex_one == edge.vertex_one && e.vertex_two == edge.vertex_two) {
            continue;
        }

        let delta = compute_delta(working_graph, clique_collection, edge, clique_size, destroyed);
        let new_cumulative = cumulative_count + delta;

        // Worsening tolerance: prune branches that go too far above baseline
        if new_cumulative - base_clique_count > config.worsening_tolerance {
            continue;
        }

        // Apply
        let destroyed_now: Vec<u32> = clique_collection
            .get_cliques_containing_edge(edge)
            .iter()
            .filter(|c| !destroyed.contains(c))
            .copied()
            .collect();
        working_graph.flip_edges(&[edge.clone()]);
        for c in &destroyed_now {
            destroyed.insert(*c);
        }
        current_sequence.push(edge.clone());

        if new_cumulative < state.best_count {
            state.best_count = new_cumulative;
            state.best_sequence = current_sequence.clone();
        }

        if current_depth < max_depth {
            search_depth(
                working_graph, clique_collection, clique_size,
                new_cumulative, current_depth + 1, max_depth,
                destroyed, current_sequence, state, config, base_clique_count,
            );
        }

        // Backtrack
        working_graph.flip_edges(&[edge.clone()]);
        for c in &destroyed_now {
            destroyed.remove(c);
        }
        current_sequence.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::get_all_cliques;

    fn make_k4_graph() -> Graph {
        // K4 complete graph on 4 vertices: all edges red
        let bitstring = "111111".to_string(); // 6 edges in upper triangle: (0,1)(0,2)(0,3)(1,2)(1,3)(2,3)
        Graph::from_bitstring(&bitstring, 4)
    }

    #[test]
    fn test_rank_edges_by_participation_orders_high_first() {
        // K4 as red-complete: all 6 edges present
        let mut graph = make_k4_graph();
        let all_cliques = get_all_cliques(&mut graph, 3);
        let mut cc = CliqueCollection::new(4);
        cc.set_cliques(all_cliques, 4);

        let ranked = rank_edges_by_participation(&graph, &cc, 10);
        assert_eq!(ranked.len(), 6); // all 6 edges of K4
        // In K4 with triangles as cliques, every edge is in exactly 2 triangles,
        // so the order is stable but participation counts are equal. Just assert length.
    }

    #[test]
    fn test_run_vds_depth_1_finds_obvious_improvement() {
        // K5 red-complete: all 10 edges present.
        // K5 has C(5,3) = 10 red triangles, 0 blue triangles.
        // Each edge of K5 is in exactly 3 triangles.
        // Flipping one edge breaks 3 red triangles and creates 0 blue triangles
        // (would need 3 missing red edges to form a blue triangle, but only 1 edge is flipped).
        // Expected: 10 - 3 = 7 triangles after flipping one edge.
        let bitstring = "1111111111".to_string(); // 10 edges in upper triangle of K5
        let mut graph = Graph::from_bitstring(&bitstring, 5);
        let all_cliques = get_all_cliques(&mut graph, 3);
        let base_total = all_cliques.len() as i32;
        let mut cc = CliqueCollection::new(5);
        cc.set_cliques(all_cliques, 5);

        let config = VdsConfig {
            max_depth: 1,
            top_first_edges: 10,
            branching_factor: 10,
            worsening_tolerance: 10000,
            random_seed: Some(42),
        };

        let result = run_vds(&graph, 3, &config, &cc, base_total);
        assert!(result.improved, "depth-1 VDS must find an improvement on K5");
        assert_eq!(result.edges_to_flip.len(), 1);
        assert!(result.final_clique_count < base_total);
        assert_eq!(result.final_clique_count, 7);
    }

    #[test]
    fn test_run_vds_depth_2_escapes_local_minimum() {
        use crate::algorithm::get_cliques_comprehensive;

        // Empty graph on 6 vertices: 0 red triangles, but C(6,3) = 20 blue triangles.
        //
        // Flipping (0,1) red: still 0 red triangles, blue triangles = 20 - 4 = 16
        // (edge (0,1) was in 4 blue triangles: {0,1,x} for x in {2,3,4,5}).
        //
        // Flipping (0,1) then (2,3) red: red triangles = 0 (no two red edges share a vertex),
        // blue triangles = 20 - 4 - 4 = 12 (no overlap between the two destroyed sets).
        //
        // So depth-1 finds (0,1) → 16, depth-2 finds (0,1)+(2,3) → 12.
        let bitstring = "000000000000000".to_string(); // 15 edges, all absent
        let mut graph = Graph::from_bitstring(&bitstring, 6);
        let all_cliques = get_all_cliques(&mut graph, 3);
        let base_total = all_cliques.len() as i32;
        let mut cc = CliqueCollection::new(6);
        cc.set_cliques(all_cliques, 6);

        let config_d1 = VdsConfig {
            max_depth: 1,
            top_first_edges: 15,
            branching_factor: 15,
            worsening_tolerance: 10000,
            random_seed: Some(42),
        };
        let result_d1 = run_vds(&graph, 3, &config_d1, &cc, base_total);

        let config_d2 = VdsConfig {
            max_depth: 2,
            top_first_edges: 15,
            branching_factor: 15,
            worsening_tolerance: 10000,
            random_seed: Some(42),
        };
        let result_d2 = run_vds(&graph, 3, &config_d2, &cc, base_total);

        assert!(result_d1.improved);
        assert!(result_d2.improved);
        assert_eq!(result_d1.final_clique_count, 16, "depth-1 should reach 16");
        assert_eq!(result_d2.final_clique_count, 12, "depth-2 should reach 12");
        assert!(
            result_d2.final_clique_count < result_d1.final_clique_count,
            "depth-2 ({}) must beat depth-1 ({})",
            result_d2.final_clique_count,
            result_d1.final_clique_count
        );
        assert_eq!(result_d2.edges_to_flip.len(), 2, "depth-2 sequence should be exactly 2 flips");

        // Independent verification: apply the reported edges to a fresh graph and
        // recount comprehensively. This must equal final_clique_count, otherwise
        // the tracker delta diverged from ground truth.
        let mut verify_graph = Graph::from_bitstring(&graph.to_bitstring(), graph.vertex_count);
        verify_graph.flip_edges(&result_d2.edges_to_flip);
        let verified = get_cliques_comprehensive(&mut verify_graph, 3);
        assert_eq!(
            verified, result_d2.final_clique_count,
            "comprehensive recount ({}) must match VDS-reported count ({})",
            verified, result_d2.final_clique_count
        );

        // Same independent check for depth-1.
        let mut verify_graph_d1 = Graph::from_bitstring(&graph.to_bitstring(), graph.vertex_count);
        verify_graph_d1.flip_edges(&result_d1.edges_to_flip);
        let verified_d1 = get_cliques_comprehensive(&mut verify_graph_d1, 3);
        assert_eq!(
            verified_d1, result_d1.final_clique_count,
            "depth-1 comprehensive recount ({}) must match VDS-reported count ({})",
            verified_d1, result_d1.final_clique_count
        );
    }

    #[test]
    fn test_evaluate_flip_matches_comprehensive_count() {
        use crate::algorithm::get_cliques_comprehensive;

        let mut graph = make_k4_graph();
        let all_cliques = get_all_cliques(&mut graph, 3);
        let base_total = all_cliques.len() as i32;
        let mut cc = CliqueCollection::new(4);
        cc.set_cliques(all_cliques, 4);

        // Recount for a clean baseline
        let verified_base = get_cliques_comprehensive(&mut graph, 3);
        assert_eq!(verified_base, base_total);

        // Flip edge (0,1) and verify evaluate_flip predicts the comprehensive result
        let edge = WorkUnitEdge { vertex_one: 0, vertex_two: 1 };
        let predicted = evaluate_flip(&mut graph, &cc, &edge, 3, base_total);

        graph.flip_edges(&[edge.clone()]);
        let actual = get_cliques_comprehensive(&mut graph, 3);
        graph.flip_edges(&[edge]); // restore

        assert_eq!(predicted, actual, "evaluate_flip prediction must match comprehensive");
    }
}
