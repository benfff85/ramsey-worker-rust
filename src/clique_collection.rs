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
        // Release the edge->cliques index entirely: counts-only readers never touch it, and an
        // all-empty Vec-per-pair still costs ~1.9 MB (79,524 Vec headers) to hold and to clone
        // for every stage. get_cliques_containing_edge() length-guards, so it returns &[].
        self.edge_to_cliques = Vec::new();
        self.cliques = Vec::new();
        self.clique_count = crate::algorithm::accumulate_edge_clique_counts(
            graph,
            clique_size,
            self.vertex_count,
            &mut self.edge_counts,
        );
    }

    /// Update the counts in place for ONE flipped edge, instead of re-traversing the whole graph.
    ///
    /// Consecutive stages differ by a single edge flip (verified in production), and only cliques
    /// containing BOTH endpoints of that edge can change — every other clique's edges are
    /// untouched. So we subtract the cliques through (u,v) in its old color and add those through
    /// it in its new color, each a seeded traversal of a small neighbourhood: ~1ms versus ~430ms
    /// for a full rebuild on an 800k-clique graph.
    ///
    /// `graph` must be the OLD graph on entry; it is flipped to the NEW graph before returning, so
    /// graph and counts stay consistent with each other.
    pub fn apply_edge_flip(
        &mut self,
        graph: &mut crate::graph::Graph,
        clique_size: usize,
        u: usize,
        v: usize,
    ) {
        // Remove the cliques the edge currently participates in (its old color).
        let old_is_red = graph.adjacency[u].get(v);
        if !old_is_red {
            graph.invert();
        }
        self.adjust_for_cliques_through(graph, clique_size, u, v, -1);
        if !old_is_red {
            graph.invert();
        }

        graph.flip_edges(&[WorkUnitEdge {
            vertex_one: u as u16,
            vertex_two: v as u16,
        }]);

        // Add the cliques it participates in now (the other color).
        let new_is_red = graph.adjacency[u].get(v);
        if !new_is_red {
            graph.invert();
        }
        self.adjust_for_cliques_through(graph, clique_size, u, v, 1);
        if !new_is_red {
            graph.invert();
        }
    }

    /// Add `delta` to every vertex pair of every clique through (u,v) in the current adjacency,
    /// and adjust the clique total by the number of cliques seen.
    fn adjust_for_cliques_through(
        &mut self,
        graph: &crate::graph::Graph,
        clique_size: usize,
        u: usize,
        v: usize,
        delta: i32,
    ) {
        let vertex_count = self.vertex_count;
        let edge_counts = &mut self.edge_counts;
        let mut seen: usize = 0;
        crate::algorithm::for_each_clique_through_edge(
            &graph.adjacency,
            u,
            v,
            clique_size,
            &mut |found| {
                seen += 1;
                // Ascending set bits, same index convention as the full build.
                let mut verts = [0usize; 32];
                let mut n = 0;
                let mut next = found.next_set_bit(0);
                while let Some(w) = next {
                    verts[n] = w;
                    n += 1;
                    next = found.next_set_bit(w + 1);
                }
                for i in 0..n {
                    let base = verts[i] * vertex_count;
                    for j in (i + 1)..n {
                        let idx = base + verts[j];
                        if idx < edge_counts.len() {
                            edge_counts[idx] += delta;
                        }
                    }
                }
            },
        );
        if delta >= 0 {
            self.clique_count += seen;
        } else {
            self.clique_count = self.clique_count.saturating_sub(seen);
        }
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
            edge_to_cliques: Vec::new(), // counts-only: see build_counts_only
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

    /// The incremental update must land on EXACTLY the counts a full rebuild of the flipped graph
    /// produces — these counts drive enumeration order, so any drift diverges the search.
    #[test]
    fn apply_edge_flip_matches_full_rebuild() {
        use crate::graph::Graph;

        let vc = 10;
        for (bits, k) in [
            ("1".repeat(45), 4),  // complete red: flipping makes a blue edge appear
            ("0".repeat(45), 4),  // complete blue
            ("101010101010101010101010101010101010101010101".to_string(), 4),
            ("110010110001110100101100011101001011000111010".to_string(), 5),
        ] {
            // Flip a handful of different edges, including both colors and both directions.
            for &(u, v) in &[(0usize, 1usize), (2, 7), (4, 5), (0, 9), (3, 8)] {
                let mut incremental_graph = Graph::from_bitstring(&bits, vc);
                let mut incremental = CliqueCollection::new(vc);
                incremental.build_counts_only(&mut incremental_graph, k);
                incremental.apply_edge_flip(&mut incremental_graph, k, u, v);

                // Ground truth: flip first, then rebuild from scratch.
                let mut expected_graph = Graph::from_bitstring(&bits, vc);
                expected_graph.flip_edges(&[WorkUnitEdge {
                    vertex_one: u as u16,
                    vertex_two: v as u16,
                }]);
                let mut expected = CliqueCollection::new(vc);
                expected.build_counts_only(&mut expected_graph, k);

                assert_eq!(
                    incremental.total(),
                    expected.total(),
                    "total mismatch after flipping ({u},{v}) with k={k}"
                );
                assert_eq!(
                    incremental.edge_counts(),
                    expected.edge_counts(),
                    "counts mismatch after flipping ({u},{v}) with k={k}"
                );
                // The graph itself must also have been advanced to the flipped state.
                assert_eq!(incremental_graph.to_bitstring(), expected_graph.to_bitstring());
            }
        }
    }

    /// Flipping the same edge twice must return to the original counts exactly (no drift).
    #[test]
    fn apply_edge_flip_is_reversible() {
        use crate::graph::Graph;
        let bits = "110010110001110100101100011101001011000111010".to_string();
        let mut g = Graph::from_bitstring(&bits, 10);
        let mut cc = CliqueCollection::new(10);
        cc.build_counts_only(&mut g, 5);
        let before_total = cc.total();
        let before_counts = cc.edge_counts().to_vec();

        cc.apply_edge_flip(&mut g, 5, 2, 7);
        cc.apply_edge_flip(&mut g, 5, 2, 7);

        assert_eq!(cc.total(), before_total);
        assert_eq!(cc.edge_counts(), before_counts.as_slice());
        assert_eq!(g.to_bitstring(), bits);
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
