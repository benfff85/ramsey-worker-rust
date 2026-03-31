use rand::Rng;

use crate::algorithm::get_cliques_comprehensive;
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
    pub max_flip_count: usize,
}

pub fn run_sa(
    base_graph: &Graph,
    clique_size: usize,
    config: &SaConfig,
    threshold: Option<i32>,
) -> SaRunResult {
    let vertex_count = base_graph.vertex_count;
    let total_edges = vertex_count * (vertex_count - 1) / 2;

    // Clone the base graph to work on
    let mut current_graph = Graph::from_bitstring(&base_graph.to_bitstring(), vertex_count);
    let initial_clique_count = get_cliques_comprehensive(&mut current_graph, clique_size);
    let mut current_clique_count = initial_clique_count;

    let mut best_graph_bitstring = current_graph.to_bitstring();
    let mut best_clique_count = initial_clique_count;

    if let Some(t) = threshold {
        log_info!(
            "SA starting: vertex_count={}, clique_size={}, initial_cliques={}, threshold={}, max_iterations={}, initial_temp={:.2}, max_flip_count={}",
            vertex_count,
            clique_size,
            initial_clique_count,
            t,
            config.max_iterations,
            config.initial_temp,
            config.max_flip_count
        );
    } else {
        log_info!(
            "SA starting: vertex_count={}, clique_size={}, initial_cliques={}, threshold=none, max_iterations={}, initial_temp={:.2}, max_flip_count={}",
            vertex_count,
            clique_size,
            initial_clique_count,
            config.max_iterations,
            config.initial_temp,
            config.max_flip_count
        );
    }

    let mut rng = rand::rng();

    for iteration in 0..config.max_iterations {
        // Linear cooling: T = initial_temp * (1 - iteration / max_iterations)
        let temp = config.initial_temp
            * (1.0 - (iteration as f64) / (config.max_iterations as f64));

        // Variable-size moves: num_flips = max(1, floor(max_flip_count * T / initial_temp))
        let num_flips = if config.initial_temp == 0.0 {
            1
        } else {
            ((config.max_flip_count as f64) * temp / config.initial_temp)
                .floor() as usize
        };
        let num_flips = num_flips.max(1);

        // Generate random edges to flip
        let edges = generate_random_edges(&mut rng, vertex_count, total_edges, num_flips);

        // Apply flips to current graph
        current_graph.flip_edges(&edges);

        // Evaluate new state
        let new_clique_count = get_cliques_comprehensive(&mut current_graph, clique_size);
        let delta = new_clique_count - current_clique_count;

        // Metropolis acceptance
        let accept = if delta <= 0 {
            true
        } else if temp == 0.0 {
            false
        } else {
            let probability = (-((delta as f64) / temp)).exp();
            rng.random::<f64>() < probability
        };

        if accept {
            current_clique_count = new_clique_count;

            // Track best
            if new_clique_count < best_clique_count {
                best_clique_count = new_clique_count;
                best_graph_bitstring = current_graph.to_bitstring();
            }
        } else {
            // Undo the flips by flipping the same edges again
            current_graph.flip_edges(&edges);
        }
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

fn generate_random_edges(
    rng: &mut impl Rng,
    vertex_count: usize,
    total_edges: usize,
    num_flips: usize,
) -> Vec<WorkUnitEdge> {
    let mut edges = Vec::with_capacity(num_flips);
    for _ in 0..num_flips {
        let edge_idx = rng.random_range(0..total_edges);
        let (u, v) = edge_index_to_vertices(edge_idx, vertex_count);
        edges.push(WorkUnitEdge {
            vertex_one: u as u16,
            vertex_two: v as u16,
        });
    }
    edges
}

fn edge_index_to_vertices(edge_idx: usize, vertex_count: usize) -> (usize, usize) {
    let mut remaining = edge_idx;
    let mut u = 0;
    loop {
        // Row u has (vertex_count - u - 1) entries
        let row_size = vertex_count - u - 1;
        if remaining < row_size {
            let v = u + 1 + remaining;
            return (u, v);
        }
        remaining -= row_size;
        u += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_index_to_vertices_basic() {
        // For vertex_count=4: edges are (0,1),(0,2),(0,3),(1,2),(1,3),(2,3)
        assert_eq!(edge_index_to_vertices(0, 4), (0, 1));
        assert_eq!(edge_index_to_vertices(1, 4), (0, 2));
        assert_eq!(edge_index_to_vertices(2, 4), (0, 3));
        assert_eq!(edge_index_to_vertices(3, 4), (1, 2));
        assert_eq!(edge_index_to_vertices(4, 4), (1, 3));
        assert_eq!(edge_index_to_vertices(5, 4), (2, 3));
    }

    #[test]
    fn test_edge_index_roundtrip() {
        let vertex_count = 6;
        let total_edges = vertex_count * (vertex_count - 1) / 2;
        let mut idx = 0;
        for u in 0..vertex_count {
            for v in (u + 1)..vertex_count {
                assert_eq!(edge_index_to_vertices(idx, vertex_count), (u, v));
                idx += 1;
            }
        }
        assert_eq!(idx, total_edges);
    }
}
