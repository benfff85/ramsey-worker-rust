use rand::Rng;

use crate::algorithm::get_cliques_comprehensive;
use crate::clique_collection::CliqueCollection;
use crate::graph::{Graph, WorkUnitEdge};
use crate::log_info;

pub struct SaRunResult {
    pub best_graph_bitstring: String,
    pub best_clique_count: i32,
    pub improved: bool,
}

pub struct SaConfig {
    pub max_iterations: u64,
    pub initial_temp: f64,
    /// Minimum number of red+blue edge pairs to flip per iteration.
    /// Should be >= 2 since exhaustive search already covers 1-pair mutations.
    pub min_pairs: usize,
    /// Maximum number of red+blue edge pairs to flip per iteration.
    pub max_pairs: usize,
}

/// Run one complete simulated annealing schedule starting from `base_graph`.
///
/// Each iteration flips a balanced set of red (present) and blue (absent) edges,
/// keeping the total edge count constant. Edge selection is guided by clique
/// participation scores from `clique_collection`: edges involved in fewer cliques
/// are preferred, since flipping them is less likely to cause large clique increases.
///
/// `clique_collection` should be built from `base_graph` before calling this function.
pub fn run_sa(
    base_graph: &Graph,
    clique_size: usize,
    config: &SaConfig,
    clique_collection: &CliqueCollection,
    threshold: Option<i32>,
) -> SaRunResult {
    let vertex_count = base_graph.vertex_count;

    let mut current_graph = Graph::from_bitstring(&base_graph.to_bitstring(), vertex_count);
    let initial_clique_count = get_cliques_comprehensive(&mut current_graph, clique_size);
    let mut current_clique_count = initial_clique_count;
    let mut best_graph_bitstring = current_graph.to_bitstring();
    let mut best_clique_count = initial_clique_count;

    // Build red (present) and blue (absent) edge lists with fixed initial weights.
    // Weight = 1 / (clique_participation + 1) so edges in fewer cliques score higher.
    // These weights are computed once from the base graph's clique structure and used
    // as a stable approximation throughout the run.
    let (mut red_edges, mut red_weights) = build_edge_list(&current_graph, true, clique_collection);
    let (mut blue_edges, mut blue_weights) = build_edge_list(&current_graph, false, clique_collection);

    if let Some(t) = threshold {
        log_info!(
            "SA starting: vertex_count={}, clique_size={}, initial_cliques={}, threshold={}, \
             max_iterations={}, initial_temp={:.2}, min_pairs={}, max_pairs={}",
            vertex_count, clique_size, initial_clique_count, t,
            config.max_iterations, config.initial_temp, config.min_pairs, config.max_pairs
        );
    } else {
        log_info!(
            "SA starting: vertex_count={}, clique_size={}, initial_cliques={}, threshold=none, \
             max_iterations={}, initial_temp={:.2}, min_pairs={}, max_pairs={}",
            vertex_count, clique_size, initial_clique_count,
            config.max_iterations, config.initial_temp, config.min_pairs, config.max_pairs
        );
    }

    let mut rng = rand::rng();

    for iteration in 0..config.max_iterations {
        // Linear cooling
        let temp = config.initial_temp
            * (1.0 - (iteration as f64) / (config.max_iterations as f64));

        // Pick a random pair count in [min_pairs, max_pairs]
        let num_pairs = if config.min_pairs == config.max_pairs {
            config.min_pairs
        } else {
            rng.random_range(config.min_pairs..=config.max_pairs)
        };

        // Select num_pairs red edges and num_pairs blue edges by weighted sampling.
        // This keeps edge count balanced (same number added as removed).
        let (selected_red_idx, selected_red) =
            weighted_sample(&mut rng, &red_edges, &red_weights, num_pairs);
        let (selected_blue_idx, selected_blue) =
            weighted_sample(&mut rng, &blue_edges, &blue_weights, num_pairs);

        let mut edges_to_flip: Vec<WorkUnitEdge> = Vec::with_capacity(num_pairs * 2);
        for &(u, v) in &selected_red {
            edges_to_flip.push(WorkUnitEdge { vertex_one: u, vertex_two: v });
        }
        for &(u, v) in &selected_blue {
            edges_to_flip.push(WorkUnitEdge { vertex_one: u, vertex_two: v });
        }

        current_graph.flip_edges(&edges_to_flip);
        let new_clique_count = get_cliques_comprehensive(&mut current_graph, clique_size);
        let delta = new_clique_count - current_clique_count;

        let accept = if delta <= 0 {
            true
        } else if temp == 0.0 {
            false
        } else {
            let probability = (-(delta as f64) / temp).exp();
            rng.random::<f64>() < probability
        };

        if accept {
            current_clique_count = new_clique_count;

            // Move flipped edges between red/blue lists, preserving their weights.
            // Sort indices descending so swap_remove doesn't shift indices we still need.
            let mut red_idx_sorted = selected_red_idx.clone();
            red_idx_sorted.sort_unstable_by(|a, b| b.cmp(a));
            for idx in red_idx_sorted {
                let e = red_edges.swap_remove(idx);
                let w = red_weights.swap_remove(idx);
                blue_edges.push(e);
                blue_weights.push(w);
            }

            let mut blue_idx_sorted = selected_blue_idx.clone();
            blue_idx_sorted.sort_unstable_by(|a, b| b.cmp(a));
            for idx in blue_idx_sorted {
                let e = blue_edges.swap_remove(idx);
                let w = blue_weights.swap_remove(idx);
                red_edges.push(e);
                red_weights.push(w);
            }

            if new_clique_count < best_clique_count {
                best_clique_count = new_clique_count;
                best_graph_bitstring = current_graph.to_bitstring();
            }
        } else {
            current_graph.flip_edges(&edges_to_flip);
        }

        log_info!(
            "SA iter {}/{}: temp={:.2}, pairs={}, new_cliques={}, accepted={}, current={}, best={}",
            iteration + 1,
            config.max_iterations,
            temp,
            num_pairs,
            new_clique_count,
            accept,
            current_clique_count,
            best_clique_count
        );
    }

    let improved = best_clique_count < initial_clique_count;
    log_info!(
        "SA finished: initial_cliques={}, best_cliques={}, improved={}",
        initial_clique_count,
        best_clique_count,
        improved
    );

    SaRunResult {
        best_graph_bitstring,
        best_clique_count,
        improved,
    }
}

/// Build a list of edges (present or absent) with participation-based weights.
/// Returns parallel (edges, weights) arrays.
fn build_edge_list(
    graph: &Graph,
    want_present: bool,
    clique_collection: &CliqueCollection,
) -> (Vec<(u16, u16)>, Vec<f64>) {
    let v = graph.vertex_count;
    let mut edges = Vec::new();
    let mut weights = Vec::new();

    for u in 0..v {
        for j in (u + 1)..v {
            if graph.adjacency[u].get(j) == want_present {
                let wu = u as u16;
                let wj = j as u16;
                let probe = [WorkUnitEdge { vertex_one: wu, vertex_two: wj }];
                let score = clique_collection.get_count_of_cliques_containing_edges(&probe);
                edges.push((wu, wj));
                weights.push(1.0 / (score as f64 + 1.0));
            }
        }
    }

    (edges, weights)
}

/// Weighted sampling without replacement. Returns (indices, edges) of the selected items.
/// Indices are into the original `edges` slice and are needed to remove the chosen
/// entries from the caller's lists on accept.
fn weighted_sample(
    rng: &mut impl Rng,
    edges: &[(u16, u16)],
    weights: &[f64],
    n: usize,
) -> (Vec<usize>, Vec<(u16, u16)>) {
    let n = n.min(edges.len());
    let mut selected_idx: Vec<usize> = Vec::with_capacity(n);
    let mut selected_edges: Vec<(u16, u16)> = Vec::with_capacity(n);

    for _ in 0..n {
        let total: f64 = weights
            .iter()
            .enumerate()
            .filter(|(i, _)| !selected_idx.contains(i))
            .map(|(_, &w)| w)
            .sum();

        if total <= 0.0 {
            break;
        }

        let mut target = rng.random::<f64>() * total;
        let mut picked = false;

        for (i, &w) in weights.iter().enumerate() {
            if selected_idx.contains(&i) {
                continue;
            }
            target -= w;
            if target <= 0.0 {
                selected_idx.push(i);
                selected_edges.push(edges[i]);
                picked = true;
                break;
            }
        }

        // Floating-point fallback: pick the first available edge
        if !picked {
            for (i, &e) in edges.iter().enumerate() {
                if !selected_idx.contains(&i) {
                    selected_idx.push(i);
                    selected_edges.push(e);
                    break;
                }
            }
        }
    }

    (selected_idx, selected_edges)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_weighted_sample_returns_correct_count() {
        let mut rng = rand::rng();
        let edges = vec![(0u16, 1u16), (0, 2), (0, 3), (1, 2), (1, 3)];
        let weights = vec![1.0, 2.0, 1.0, 3.0, 1.0];
        let (idx, selected) = weighted_sample(&mut rng, &edges, &weights, 3);
        assert_eq!(selected.len(), 3);
        assert_eq!(idx.len(), 3);
        // No duplicates
        let mut sorted = idx.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 3);
    }

    #[test]
    fn test_weighted_sample_capped_by_edge_count() {
        let mut rng = rand::rng();
        let edges = vec![(0u16, 1u16), (0, 2)];
        let weights = vec![1.0, 1.0];
        let (_, selected) = weighted_sample(&mut rng, &edges, &weights, 5);
        assert_eq!(selected.len(), 2); // Can't return more than available
    }
}
