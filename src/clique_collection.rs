use crate::graph::WorkUnitEdge;

#[derive(Debug, Clone)]
pub struct CliqueCollection {
    // Flat array to map edge to the number of cliques it belongs to.
    // Indexing: u * vertex_count + v (assuming u < v)
    edge_counts: Vec<i32>,
    // Parallel to edge_counts: for each edge slot, list of clique indices containing that edge.
    edge_to_cliques: Vec<Vec<u32>>,
    vertex_count: usize,
    clique_count: usize,
    cliques: Vec<Vec<usize>>,
}

impl CliqueCollection {
    pub fn new(vertex_count: usize) -> Self {
        // Size for N vertices should be N * N to cover all pairs simply
        let size = vertex_count * vertex_count;
        let edge_counts = vec![0; size];
        let edge_to_cliques = vec![Vec::new(); size];

        CliqueCollection {
            edge_counts,
            edge_to_cliques,
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
        for v in &mut self.edge_to_cliques {
            v.clear();
        }

        for (clique_idx, clique) in self.cliques.iter().enumerate() {
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
                        self.edge_to_cliques[idx].push(clique_idx as u32);
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

    pub fn get_cliques_containing_edge(&self, edge: &WorkUnitEdge) -> &[u32] {
        let u = edge.vertex_one as usize;
        let v = edge.vertex_two as usize;
        let (min, max) = if u < v { (u, v) } else { (v, u) };
        let idx = min * self.vertex_count + max;
        if idx < self.edge_to_cliques.len() {
            &self.edge_to_cliques[idx]
        } else {
            &[]
        }
    }

    pub fn cliques(&self) -> &[Vec<usize>] {
        &self.cliques
    }

    pub fn total(&self) -> usize {
        self.clique_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_edge_to_cliques_lookup_on_k4() {
        // K4 has 4 triangles (3-cliques); each edge is in exactly 2 triangles
        let cliques = vec![
            vec![0, 1, 2],
            vec![0, 1, 3],
            vec![0, 2, 3],
            vec![1, 2, 3],
        ];
        let mut cc = CliqueCollection::new(4);
        cc.set_cliques(cliques, 4);

        let edge = WorkUnitEdge { vertex_one: 0, vertex_two: 1 };
        let containing = cc.get_cliques_containing_edge(&edge);
        // Edge (0,1) is in cliques 0 and 1 ({0,1,2} and {0,1,3})
        assert_eq!(containing.len(), 2);
        assert!(containing.contains(&0));
        assert!(containing.contains(&1));
    }
}
