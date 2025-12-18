use crate::graph::WorkUnitEdge;

#[derive(Debug, Clone)]
pub struct CliqueCollection {
    // Flat array to map edge to the number of cliques it belongs to.
    // Indexing: u * vertex_count + v (assuming u < v)
    edge_counts: Vec<i32>,
    vertex_count: usize,
    clique_count: usize,
    cliques: Vec<Vec<usize>>,
}

impl CliqueCollection {
    pub fn new(vertex_count: usize) -> Self {
        // Size for N vertices should be N * N to cover all pairs simply
        let size = vertex_count * vertex_count;
        let edge_counts = vec![0; size];

        CliqueCollection {
            edge_counts,
            vertex_count,
            clique_count: 0,
            cliques: Vec::new(),
        }
    }

    pub fn set_cliques(&mut self, input_cliques: Vec<Vec<usize>>, vertex_count: usize) {
        self.cliques = input_cliques;
        self.clique_count = self.cliques.len();
        self.vertex_count = vertex_count;

        // Reset counts
        // Much faster to standard fill for vec
        self.edge_counts.fill(0);

        for clique in &self.cliques {
            let size = clique.len();
            for i in 0..size {
                let u = clique[i];
                for j in (i + 1)..size {
                    let v = clique[j];

                    // Always order u < v for consistent indexing
                    let (min, max) = if u < v { (u, v) } else { (v, u) };
                    let idx = min * self.vertex_count + max;

                    // Safety check not strictly needed if we trust inputs but good for panic avoidance
                    if idx < self.edge_counts.len() {
                        self.edge_counts[idx] += 1;
                    }
                }
            }
        }
    }

    pub fn get_count_of_cliques_containing_edges(&self, edges: &[WorkUnitEdge]) -> i32 {
        let mut sum = 0;
        for edge in edges {
            let u = edge.vertex_one as usize;
            let v = edge.vertex_two as usize;
            let (min, max) = if u < v { (u, v) } else { (v, u) };

            let idx = min * self.vertex_count + max;

            if idx < self.edge_counts.len() {
                sum += self.edge_counts[idx];
            }
        }
        sum
    }

    pub fn total(&self) -> usize {
        self.clique_count
    }
}
