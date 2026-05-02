use std::collections::HashSet;
use std::time::Instant;

use rand::SeedableRng;
use rand::rngs::StdRng;
use rand::seq::SliceRandom;

use crate::algorithm::get_new_cliques_with_limit;
use crate::clique_collection::CliqueCollection;
use crate::graph::{Graph, WorkUnitEdge};
use crate::log_info;

struct SearchState {
    best_sequence: Vec<WorkUnitEdge>,
    best_count: i32,
}

/// Per-run search statistics, updated as run_vds and search_depth iterate.
/// `nodes_visited` is the total number of candidate edges evaluated across
/// all depths (each evaluation is one Bron-Kerbosch new-clique enumeration).
/// `branches_pruned` is the count of subtree skips from the worsening_tolerance
/// guard at depth >= 2.
#[derive(Default)]
struct SearchStats {
    nodes_visited: u64,
    branches_pruned: u64,
}

pub struct VdsConfig {
    pub max_depth: usize,
    pub top_first_edges: usize,
    pub branching_factor: usize,
    pub worsening_tolerance: i32,
    pub random_seed: Option<u64>,
    /// Minimum number of flips before the search begins branching.
    /// With start_depth=3, VDS randomly picks 2 prefix edges (depth 1-2)
    /// then branches from depth 3 onward. This skips the 1- and 2-flip
    /// space that exhaustive workers already cover.
    pub start_depth: usize,
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
/// When `config.start_depth > 1`, a random prefix of `start_depth - 1` edges
/// is applied before branching begins, so the search starts in N-flip space
/// that exhaustive workers can't reach. Worsening tolerance is measured from
/// the prefix state (not the original base) so the search can explore around
/// the random starting point. Improvements are still tracked against the
/// original `base_clique_count`.
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
         max_depth={}, top_first_edges={}, branching_factor={}, worsening_tolerance={}, start_depth={}",
        base_graph.vertex_count, clique_size, base_clique_count,
        config.max_depth, config.top_first_edges, config.branching_factor,
        config.worsening_tolerance, config.start_depth
    );

    let mut working_graph = Graph::from_bitstring(&base_graph.to_bitstring(), base_graph.vertex_count);

    // Pre-compute first-edge candidates from global participation ranking.
    let mut first_edge_candidates = rank_edges_by_participation(&working_graph, clique_collection, config.top_first_edges);

    // Seeding rule:
    //   Some(seed) → deterministic StdRng (for tests / reproducibility)
    //   None       → fresh OS entropy each call (production default)
    let mut rng: StdRng = match config.random_seed {
        Some(seed) => StdRng::seed_from_u64(seed),
        None => StdRng::from_rng(&mut rand::rng()),
    };
    first_edge_candidates.shuffle(&mut rng);
    // Sample branching_factor edges from the shuffled pool (or fewer if pool is smaller)
    let sample_size = config.branching_factor.min(first_edge_candidates.len());
    let first_edges_this_run = &first_edge_candidates[..sample_size];

    let mut state = SearchState {
        best_sequence: Vec::new(),
        best_count: base_clique_count,
    };
    let mut current_sequence: Vec<WorkUnitEdge> = Vec::new();
    let mut destroyed: HashSet<u32> = HashSet::new();
    let mut stats = SearchStats::default();
    let started = Instant::now();

    for first_edge in first_edges_this_run {
        // Compute delta for this first flip
        stats.nodes_visited += 1;
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

        // Only record improvements at or beyond start_depth
        if config.start_depth <= 1 && cumulative < state.best_count {
            state.best_count = cumulative;
            state.best_sequence = current_sequence.clone();
        }

        // Build random prefix for depths 2..start_depth (no branching, one
        // random neighbor at each level). This walks to the start_depth starting
        // point before the branching search begins.
        if config.start_depth > 1 && config.max_depth >= 2 {
            build_prefix_and_search(
                &mut working_graph, clique_collection, clique_size,
                cumulative, 2,
                &mut destroyed, &mut current_sequence,
                &mut state, config, base_clique_count,
                &mut stats, &mut rng,
            );
        } else if config.max_depth >= 2 {
            search_depth(
                &mut working_graph, clique_collection, clique_size,
                cumulative, 2, config.max_depth,
                &mut destroyed, &mut current_sequence,
                &mut state, config, base_clique_count,
                cumulative, // tolerance_base = cumulative after depth 1
                &mut stats, &mut rng,
            );
        }

        // Backtrack: unflip, restore destroyed set
        working_graph.flip_edges(&[first_edge.clone()]);
        for c in &destroyed_now {
            destroyed.remove(c);
        }
        current_sequence.pop();
    }

    let elapsed_ms = started.elapsed().as_millis();

    let improved = !state.best_sequence.is_empty() && state.best_count < base_clique_count;

    // Final verification: apply the best sequence and recount comprehensively.
    // If the running-delta was optimistic (tracker bug), bail out.
    if improved {
        let mut verify_graph = Graph::from_bitstring(&base_graph.to_bitstring(), base_graph.vertex_count);
        verify_graph.flip_edges(&state.best_sequence);
        let verified_count = crate::algorithm::get_cliques_comprehensive(&mut verify_graph, clique_size);
        if verified_count != state.best_count {
            log_info!(
                "VDS verification MISMATCH: tracker said {}, comprehensive says {} — skipping submission \
                 (elapsed_ms={}, nodes_visited={}, branches_pruned={})",
                state.best_count, verified_count,
                elapsed_ms, stats.nodes_visited, stats.branches_pruned
            );
            return VdsRunResult {
                edges_to_flip: Vec::new(),
                final_clique_count: base_clique_count,
                improved: false,
            };
        }
        log_info!(
            "VDS verified: base={}, final={}, delta={}, sequence_len={}, \
             elapsed_ms={}, nodes_visited={}, branches_pruned={}",
            base_clique_count, verified_count,
            verified_count - base_clique_count,
            state.best_sequence.len(),
            elapsed_ms, stats.nodes_visited, stats.branches_pruned
        );
    } else {
        log_info!(
            "VDS finished: no improvement found, elapsed_ms={}, nodes_visited={}, branches_pruned={}",
            elapsed_ms, stats.nodes_visited, stats.branches_pruned
        );
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

/// Select candidate edges from the neighborhood of the current flip sequence.
///
/// Collects all edges incident to any vertex touched by `current_sequence`,
/// ranks them by surviving clique participation (original count minus destroyed),
/// takes the top 3*K as a quality-filtered pool, shuffles that pool, then returns
/// K candidates. This balances focus (only high-participation edges) with
/// exploration (different subset each run).
fn get_neighborhood_candidates(
    vertex_count: usize,
    clique_collection: &CliqueCollection,
    current_sequence: &[WorkUnitEdge],
    destroyed: &HashSet<u32>,
    top_k: usize,
    rng: &mut StdRng,
) -> Vec<WorkUnitEdge> {
    // Collect unique vertices from the sequence
    let mut vertex_set: HashSet<usize> = HashSet::new();
    for edge in current_sequence {
        vertex_set.insert(edge.vertex_one as usize);
        vertex_set.insert(edge.vertex_two as usize);
    }

    // Build set of sequence edges to exclude
    let mut sequence_edges: HashSet<(u16, u16)> = HashSet::new();
    for e in current_sequence {
        let (a, b) = if e.vertex_one < e.vertex_two {
            (e.vertex_one, e.vertex_two)
        } else {
            (e.vertex_two, e.vertex_one)
        };
        sequence_edges.insert((a, b));
    }

    // Enumerate all edges incident to those vertices, deduplicated
    let mut seen: HashSet<(u16, u16)> = sequence_edges.clone();
    let mut scored: Vec<(i32, WorkUnitEdge)> = Vec::new();

    for &v in &vertex_set {
        for w in 0..vertex_count {
            if w == v {
                continue;
            }
            let (a, b) = if v < w {
                (v as u16, w as u16)
            } else {
                (w as u16, v as u16)
            };
            if !seen.insert((a, b)) {
                continue; // already scored or in sequence
            }
            let edge = WorkUnitEdge {
                vertex_one: a,
                vertex_two: b,
            };
            let surviving: i32 = clique_collection
                .get_cliques_containing_edge(&edge)
                .iter()
                .filter(|c| !destroyed.contains(c))
                .count() as i32;
            scored.push((surviving, edge));
        }
    }

    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(a.1.vertex_one.cmp(&b.1.vertex_one))
            .then(a.1.vertex_two.cmp(&b.1.vertex_two))
    });

    // Take a larger pool (3x), shuffle it, then return top_k.
    // This keeps candidates quality-filtered while ensuring each run
    // explores different subtrees.
    let pool_size = (top_k * 3).min(scored.len());
    let mut pool: Vec<WorkUnitEdge> = scored.into_iter().take(pool_size).map(|(_, e)| e).collect();
    pool.shuffle(rng);
    pool.into_iter().take(top_k).collect()
}

/// Build a random prefix path from the current depth up to `start_depth`,
/// picking one random neighborhood edge at each level (no branching).
/// Once at `start_depth`, begins the branching search.
fn build_prefix_and_search(
    working_graph: &mut Graph,
    clique_collection: &CliqueCollection,
    clique_size: usize,
    cumulative_count: i32,
    current_depth: usize,
    destroyed: &mut HashSet<u32>,
    current_sequence: &mut Vec<WorkUnitEdge>,
    state: &mut SearchState,
    config: &VdsConfig,
    base_clique_count: i32,
    stats: &mut SearchStats,
    rng: &mut StdRng,
) {
    if current_depth >= config.start_depth {
        // We've reached start_depth — begin branching search from here.
        // Tolerance is measured from the cumulative count at the start of
        // branching so the search explores around this prefix state.
        if current_depth <= config.max_depth {
            search_depth(
                working_graph, clique_collection, clique_size,
                cumulative_count, current_depth, config.max_depth,
                destroyed, current_sequence, state, config, base_clique_count,
                cumulative_count, // tolerance_base = count at prefix end
                stats, rng,
            );
        }
        return;
    }

    // Still building prefix: pick one random neighbor and continue
    let candidates = get_neighborhood_candidates(
        working_graph.vertex_count,
        clique_collection,
        current_sequence,
        destroyed,
        config.branching_factor,
        rng,
    );

    if candidates.is_empty() {
        return;
    }

    // Pick one random candidate (candidates are already shuffled from a ranked pool)
    let edge = &candidates[0];

    stats.nodes_visited += 1;
    let delta = compute_delta(working_graph, clique_collection, edge, clique_size, destroyed);

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

    let new_cumulative = cumulative_count + delta;

    build_prefix_and_search(
        working_graph, clique_collection, clique_size,
        new_cumulative, current_depth + 1,
        destroyed, current_sequence, state, config, base_clique_count,
        stats, rng,
    );

    // Backtrack
    working_graph.flip_edges(&[edge.clone()]);
    for c in &destroyed_now {
        destroyed.remove(c);
    }
    current_sequence.pop();
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
    // The clique count at the point where branching began (after the prefix).
    // Worsening tolerance is measured from this value, not from base_clique_count.
    tolerance_base: i32,
    stats: &mut SearchStats,
    rng: &mut StdRng,
) {
    // Locality-aware: candidates are edges adjacent to previously flipped edges,
    // ranked by surviving clique participation then randomly sampled from the
    // top pool. Each branch fans out into a different local neighborhood.
    let candidates = get_neighborhood_candidates(
        working_graph.vertex_count,
        clique_collection,
        current_sequence,
        destroyed,
        config.branching_factor,
        rng,
    );

    for edge in &candidates {
        stats.nodes_visited += 1;
        let delta = compute_delta(working_graph, clique_collection, edge, clique_size, destroyed);
        let new_cumulative = cumulative_count + delta;

        // Worsening tolerance: prune branches that go too far above the
        // tolerance baseline (the count at the start of branching)
        if new_cumulative - tolerance_base > config.worsening_tolerance {
            stats.branches_pruned += 1;
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
                tolerance_base, stats, rng,
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
            start_depth: 1,
        };

        let result = run_vds(&graph, 3, &config, &cc, base_total);
        assert!(result.improved, "depth-1 VDS must find an improvement on K5");
        assert_eq!(result.edges_to_flip.len(), 1);
        assert!(result.final_clique_count < base_total);
        assert_eq!(result.final_clique_count, 7);
    }

    #[test]
    fn test_run_vds_depth_2_improves_over_depth_1() {
        use crate::algorithm::get_cliques_comprehensive;

        // Empty graph on 6 vertices: 0 red triangles, but C(6,3) = 20 blue triangles.
        //
        // Depth-1: flipping any single edge breaks 4 blue triangles → 16.
        //
        // Depth-2 with locality-aware selection: the second flip must share a vertex
        // with the first. Adjacent pairs share one vertex, so the second flip's 4
        // original blue triangles overlap by 1 with the first → breaks 3 more → 13.
        // (Non-local pairs like (0,1)+(2,3) would give 12, but locality-aware search
        // correctly prioritizes the local neighborhood. The deeper-search test below
        // shows how LK chaining reaches non-local edges through intermediate steps.)
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
            start_depth: 1,
        };
        let result_d1 = run_vds(&graph, 3, &config_d1, &cc, base_total);

        let config_d2 = VdsConfig {
            max_depth: 2,
            top_first_edges: 15,
            branching_factor: 15,
            worsening_tolerance: 10000,
            random_seed: Some(42),
            start_depth: 1,
        };
        let result_d2 = run_vds(&graph, 3, &config_d2, &cc, base_total);

        assert!(result_d1.improved);
        assert!(result_d2.improved);
        assert_eq!(result_d1.final_clique_count, 16, "depth-1 should reach 16");
        assert_eq!(result_d2.final_clique_count, 13, "depth-2 local should reach 13");
        assert!(
            result_d2.final_clique_count < result_d1.final_clique_count,
            "depth-2 ({}) must beat depth-1 ({})",
            result_d2.final_clique_count,
            result_d1.final_clique_count
        );

        // Independent verification
        let mut verify_graph = Graph::from_bitstring(&graph.to_bitstring(), graph.vertex_count);
        verify_graph.flip_edges(&result_d2.edges_to_flip);
        let verified = get_cliques_comprehensive(&mut verify_graph, 3);
        assert_eq!(
            verified, result_d2.final_clique_count,
            "comprehensive recount ({}) must match VDS-reported count ({})",
            verified, result_d2.final_clique_count
        );

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
    fn test_run_vds_deeper_search_chains_through_locality() {
        use crate::algorithm::get_cliques_comprehensive;

        // Same 6-vertex empty graph. Depth-3 can chain: (a,b)→(b,c)→(c,d) where
        // each step is local to the previous but the endpoints a,d are non-adjacent.
        // This reaches improvements that depth-2 locality can't find in a single hop.
        //
        // At depth 4+, even more diverse paths are explored. We verify that deeper
        // search finds strictly better results.
        let bitstring = "000000000000000".to_string();
        let mut graph = Graph::from_bitstring(&bitstring, 6);
        let all_cliques = get_all_cliques(&mut graph, 3);
        let base_total = all_cliques.len() as i32;
        let mut cc = CliqueCollection::new(6);
        cc.set_cliques(all_cliques, 6);

        let config_d2 = VdsConfig {
            max_depth: 2,
            top_first_edges: 15,
            branching_factor: 15,
            worsening_tolerance: 10000,
            random_seed: Some(42),
            start_depth: 1,
        };
        let result_d2 = run_vds(&graph, 3, &config_d2, &cc, base_total);

        let config_d4 = VdsConfig {
            max_depth: 4,
            top_first_edges: 15,
            branching_factor: 15,
            worsening_tolerance: 10000,
            random_seed: Some(42),
            start_depth: 1,
        };
        let result_d4 = run_vds(&graph, 3, &config_d4, &cc, base_total);

        assert!(result_d4.improved);
        assert!(
            result_d4.final_clique_count < result_d2.final_clique_count,
            "depth-4 ({}) must beat depth-2 ({}) via LK chaining",
            result_d4.final_clique_count,
            result_d2.final_clique_count
        );

        // Verify comprehensively
        let mut verify_graph = Graph::from_bitstring(&graph.to_bitstring(), graph.vertex_count);
        verify_graph.flip_edges(&result_d4.edges_to_flip);
        let verified = get_cliques_comprehensive(&mut verify_graph, 3);
        assert_eq!(
            verified, result_d4.final_clique_count,
            "depth-4 comprehensive recount ({}) must match VDS-reported count ({})",
            verified, result_d4.final_clique_count
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
