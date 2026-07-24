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

    /// Build ONLY the per-edge cardinality counts and the clique total, by streaming the
    /// Bron-Kerbosch traversal instead of materializing the clique list.
    ///
    /// The counter-based worker path reads only [`Self::get_count_of_cliques_containing_edges`]
    /// and [`Self::total`], so this skips ~300 MB of allocation (the clique list plus the
    /// edge->cliques index) per stage. On a counts-only collection
    /// [`Self::get_cliques_containing_edge`] and [`Self::cliques`] are EMPTY — modes that need
    /// them (tabu/vds/sa) must keep using [`Self::set_cliques`].
    pub fn build_counts_only(&mut self, graph: &mut crate::graph::Graph, clique_size: usize) {
        self.edge_counts.fill(0);
        for v in &mut self.edge_to_cliques {
            v.clear();
        }
        self.cliques = Vec::new();
        self.clique_count = crate::algorithm::accumulate_edge_clique_counts(
            graph,
            clique_size,
            self.vertex_count,
            &mut self.edge_counts,
        );
    }

    /// Rehydrate a counts-only collection from counts computed elsewhere (e.g. shared by a
    /// peer worker via Redis), skipping the traversal entirely.
    pub fn from_shared_counts(
        vertex_count: usize,
        edge_counts: Vec<i32>,
        clique_count: usize,
    ) -> Self {
        let size = vertex_count * vertex_count;
        let mut counts = edge_counts;
        counts.resize(size, 0);
        CliqueCollection {
            edge_counts: counts,
            edge_to_cliques: vec![Vec::new(); size],
            vertex_count,
            clique_count,
            cliques: Vec::new(),
        }
    }

    /// Raw per-edge counts, for sharing with peer workers.
    pub fn edge_counts(&self) -> &[i32] {
        &self.edge_counts
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

    /// The whole point of counts-only: it must produce EXACTLY the per-edge counts and total
    /// that the old collect-then-index path produced, since those drive enumeration ordering.
    #[test]
    fn build_counts_only_matches_set_cliques() {
        use crate::algorithm::get_all_cliques;
        use crate::graph::Graph;

        // A few graph shapes, both colors exercised, k=4 and k=5.
        for (bits, vc, k) in [
            ("1".repeat(45), 10, 4),   // complete red K10
            ("0".repeat(45), 10, 4),   // complete blue K10
            ("101010101010101010101010101010101010101010101".to_string(), 10, 4),
            ("110010110001110100101100011101001011000111010".to_string(), 10, 5),
        ] {
            let mut g1 = Graph::from_bitstring(&bits, vc);
            let mut expected = CliqueCollection::new(vc);
            expected.set_cliques(get_all_cliques(&mut g1, k), vc);

            let mut g2 = Graph::from_bitstring(&bits, vc);
            let mut actual = CliqueCollection::new(vc);
            actual.build_counts_only(&mut g2, k);

            assert_eq!(actual.total(), expected.total(), "total mismatch for k={k}");
            assert_eq!(
                actual.edge_counts(),
                expected.edge_counts(),
                "per-edge counts mismatch for k={k}"
            );
        }
    }

    #[test]
    fn from_shared_counts_round_trips_lookups() {
        use crate::graph::Graph;
        let bits = "1".repeat(45);
        let mut g = Graph::from_bitstring(&bits, 10);
        let mut built = CliqueCollection::new(10);
        built.build_counts_only(&mut g, 4);

        let shared =
            CliqueCollection::from_shared_counts(10, built.edge_counts().to_vec(), built.total());
        assert_eq!(shared.total(), built.total());
        let edge = WorkUnitEdge { vertex_one: 0, vertex_two: 1 };
        assert_eq!(
            shared.get_count_of_cliques_containing_edges(std::slice::from_ref(&edge)),
            built.get_count_of_cliques_containing_edges(std::slice::from_ref(&edge))
        );
    }

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
