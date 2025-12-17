use crate::graph::WorkUnitEdge;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct CliqueCollection {
    // Map edge to the number of cliques it belongs to
    edge_map: HashMap<(i32, i32), i32>,
    clique_count: usize,
    cliques: Vec<Vec<usize>>,
}

impl CliqueCollection {
    pub fn new(vertex_count: usize) -> Self {
        let mut edge_map = HashMap::new();
        // Pre-initialize edge map for all possible edges
        // This mirrors the Java implementation logic, although Java uses Short.
        // We use (min, max) tuple as key to ensure canonical representation.
        for i in 0..vertex_count {
            for j in (i + 1)..vertex_count {
                edge_map.insert((i as i32, j as i32), 0);
            }
        }

        CliqueCollection {
            edge_map,
            clique_count: 0,
            cliques: Vec::new(),
        }
    }

    pub fn set_cliques(&mut self, input_cliques: Vec<Vec<usize>>, _vertex_count: usize) {
        self.cliques = input_cliques;
        self.clique_count = self.cliques.len();

        // Reset edge map
        // Re-populating might be faster than iterating to clear if map is large vs dense?
        // But let's follow the clear-then-fill pattern or just re-create.
        // The Java code iterates keys to reset to 0.
        // We can just clear and loop over the keys we have, OR re-create.
        // Re-creating the map requires recreating all keys.
        // Let's iterate and zero out.
        for val in self.edge_map.values_mut() {
            *val = 0;
        }

        // Use a temporary edge to minimize allocation if we were keyed by object,
        // but here we are keyed by tuple value so it's cheap.

        for clique in &self.cliques {
            let size = clique.len();
            for i in 0..size {
                let u = clique[i] as i32;
                for j in (i + 1)..size {
                    let v = clique[j] as i32;
                    let key = if u < v { (u, v) } else { (v, u) };

                    self.edge_map
                        .entry(key)
                        .and_modify(|count| *count += 1)
                        .or_insert(1); // Should theoretically always hit valid key if initialized
                }
            }
        }
    }

    pub fn get_count_of_cliques_containing_edges(&self, edges: &[WorkUnitEdge]) -> i32 {
        let mut sum = 0;
        for edge in edges {
            let u = edge.vertex_one as i32;
            let v = edge.vertex_two as i32;
            let key = if u < v { (u, v) } else { (v, u) };

            if let Some(count) = self.edge_map.get(&key) {
                sum += count;
            }
        }
        sum
    }

    pub fn total(&self) -> usize {
        self.clique_count
    }
}
