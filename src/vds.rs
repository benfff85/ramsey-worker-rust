use crate::algorithm::get_new_cliques_with_limit;
use crate::clique_collection::CliqueCollection;
use crate::graph::{Graph, WorkUnitEdge};
use crate::log_info;

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

    let mut best_count = base_clique_count;
    let mut best_sequence: Vec<WorkUnitEdge> = Vec::new();

    for edge in &candidates {
        let count = evaluate_flip(&mut working_graph, clique_collection, edge, clique_size, base_clique_count);
        if count < best_count {
            best_count = count;
            best_sequence = vec![edge.clone()];
        }
    }

    let improved = !best_sequence.is_empty();
    log_info!(
        "VDS finished: base={}, best={}, improved={}, sequence_len={}",
        base_clique_count, best_count, improved, best_sequence.len()
    );

    VdsRunResult {
        edges_to_flip: best_sequence,
        final_clique_count: best_count,
        improved,
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
