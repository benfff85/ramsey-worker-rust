use crate::bitset::BitMatrix;
use crate::graph::{Graph, WorkUnitEdge};

/// Replicates TargetedCliqueCheckServiceBitSet.getNewCliques
/// Counts new cliques formed after edge flips, using seeded Bron-Kerbosch.
/// Uses the optimized no-X variant since we start with empty X.
///
/// Early termination: if `threshold` is provided and count exceeds it,
/// stops counting and returns early (since we can't improve on best).
pub fn get_new_cliques_with_limit(
    graph: &mut Graph,
    clique_size: usize,
    flipped_edges: &[WorkUnitEdge],
    threshold: i32,
) -> (i32, bool) {
    let mut new_clique_count = 0;
    let mut exceeded = false;

    // Check RED (current adjacency)
    for edge in flipped_edges {
        if exceeded {
            break;
        }
        let v1 = edge.vertex_one as usize;
        let v2 = edge.vertex_two as usize;

        if graph.adjacency[v1].get(v2) {
            let mut p = graph.adjacency[v1];
            p.and_assign(&graph.adjacency[v2]);
            p.clear(v1);
            p.clear(v2);

            // Use the no-X variant with limit for targeted search; the two seed
            // vertices are accounted for by starting at depth 2.
            let remaining = threshold - new_clique_count;
            let (count, over) =
                bron_kerbosch_count_no_x_with_limit(2, &mut p, &graph.adjacency, clique_size, remaining);
            new_clique_count += count;
            if over {
                exceeded = true;
            }
        }
    }

    // Check BLUE (inverted adjacency)
    graph.invert();
    for edge in flipped_edges {
        if exceeded {
            break;
        }
        let v1 = edge.vertex_one as usize;
        let v2 = edge.vertex_two as usize;

        if graph.adjacency[v1].get(v2) {
            let mut p = graph.adjacency[v1];
            p.and_assign(&graph.adjacency[v2]);
            p.clear(v1);
            p.clear(v2);

            let remaining = threshold - new_clique_count;
            let (count, over) =
                bron_kerbosch_count_no_x_with_limit(2, &mut p, &graph.adjacency, clique_size, remaining);
            new_clique_count += count;
            if over {
                exceeded = true;
            }
        }
    }

    // Restore graph state
    graph.invert();

    (new_clique_count, exceeded)
}

/// Original version without early termination (for base graph enumeration)
pub fn get_new_cliques(
    graph: &mut Graph,
    clique_size: usize,
    flipped_edges: &[WorkUnitEdge],
) -> i32 {
    let (count, _) = get_new_cliques_with_limit(graph, clique_size, flipped_edges, i32::MAX);
    count
}

pub fn get_all_cliques(graph: &mut Graph, clique_size: usize) -> Vec<Vec<usize>> {
    let mut cliques = Vec::new();

    // RED
    let mut r = BitMatrix::new();
    let mut p = BitMatrix::new();
    let mut x = BitMatrix::new();

    // Set all bits in P to 1
    for i in 0..graph.vertex_count {
        p.set(i);
    }

    bron_kerbosch_collect_inplace(
        &mut r,
        &mut p,
        &mut x,
        &graph.adjacency,
        clique_size,
        &mut cliques,
    );

    // BLUE
    graph.invert();

    // Reset for Blue pass
    let mut r_blue = BitMatrix::new();
    let mut p_blue = BitMatrix::new();
    let mut x_blue = BitMatrix::new();
    for i in 0..graph.vertex_count {
        p_blue.set(i);
    }

    bron_kerbosch_collect_inplace(
        &mut r_blue,
        &mut p_blue,
        &mut x_blue,
        &graph.adjacency,
        clique_size,
        &mut cliques,
    );

    graph.invert(); // Restore

    cliques
}

/// Bron-Kerbosch with in-place mutation and backtracking.
fn bron_kerbosch_collect_inplace(
    r: &mut BitMatrix,
    p: &mut BitMatrix,
    x: &mut BitMatrix,
    adjacency: &[BitMatrix],
    clique_size: usize,
    cliques: &mut Vec<Vec<usize>>,
) {
    if r.cardinality() as usize == clique_size {
        cliques.push(r.to_indices());
        return;
    }

    if ((r.cardinality() + p.cardinality()) as usize) < clique_size {
        return;
    }

    if p.is_empty() {
        return;
    }

    let candidates = *p;

    let mut v_opt = candidates.next_set_bit(0);
    while let Some(v) = v_opt {
        r.set(v);

        let mut new_p = *p;
        new_p.and_assign(&adjacency[v]);

        let mut new_x = *x;
        new_x.and_assign(&adjacency[v]);

        bron_kerbosch_collect_inplace(r, &mut new_p, &mut new_x, adjacency, clique_size, cliques);

        r.clear(v);
        p.clear(v);
        x.set(v);

        v_opt = candidates.next_set_bit(v + 1);
    }
}

pub fn get_cliques_comprehensive(graph: &mut Graph, clique_size: usize) -> i32 {
    let mut clique_count = 0;

    // RED
    let mut p = BitMatrix::new();
    for i in 0..graph.vertex_count {
        p.set(i);
    }
    clique_count += bron_kerbosch_count_inplace(0, &mut p, &graph.adjacency, clique_size);

    // BLUE
    graph.invert();

    let mut p_blue = BitMatrix::new();
    for i in 0..graph.vertex_count {
        p_blue.set(i);
    }
    clique_count += bron_kerbosch_count_inplace(0, &mut p_blue, &graph.adjacency, clique_size);
    graph.invert(); // Restore

    clique_count
}

/// Bron-Kerbosch counting of ALL k-cliques. Duplicates are prevented by the
/// shrinking candidate set (each level only extends with later candidates), so
/// no R set is needed (its size is `depth`) and no X set is needed (X exists to
/// detect maximality, which all-k-clique counting never checks).
#[inline]
fn bron_kerbosch_count_inplace(
    depth: usize,
    p: &mut BitMatrix,
    adjacency: &[BitMatrix],
    clique_size: usize,
) -> i32 {
    if depth == clique_size {
        return 1;
    }

    let p_card = p.cardinality() as usize;

    // Leaf shortcut: P is the set of common neighbors of every committed
    // vertex, so with one slot left each candidate completes exactly one
    // k-clique — one popcount replaces |P| recursions.
    if depth == clique_size - 1 {
        return p_card as i32;
    }

    // Subsumes the empty-P check: depth < clique_size - 1 here, so p_card = 0
    // always fails this bound.
    if depth + p_card < clique_size {
        return 0;
    }

    // Second-to-last level inline: each child would immediately take the leaf
    // shortcut, so fold it in — one AND + popcount per candidate, no recursion.
    if depth == clique_size - 2 {
        let mut count = 0;
        let candidates = *p;
        for v in candidates.iter_set_bits() {
            let mut pv = *p;
            pv.and_assign(&adjacency[v]);
            count += pv.cardinality() as i32;
            p.clear(v);
        }
        return count;
    }

    let mut count = 0;
    let candidates = *p;
    for v in candidates.iter_set_bits() {
        let mut new_p = *p;
        new_p.and_assign(&adjacency[v]);
        count += bron_kerbosch_count_inplace(depth + 1, &mut new_p, adjacency, clique_size);
        p.clear(v);
    }

    count
}

/// Seeded Bron-Kerbosch counting with early termination when `limit` is
/// exceeded. `depth` is the number of vertices already committed to the clique
/// (the caller seeds an edge, so it starts at 2); no R or X set is carried —
/// R's only observable property is its size, and X is never consulted when
/// counting all k-cliques.
/// Returns (count, exceeded) — if exceeded is true, count may be incomplete
/// (or overshoot the old level-by-level cutoff) but is known to beat `limit`;
/// callers discard it.
#[inline]
fn bron_kerbosch_count_no_x_with_limit(
    depth: usize,
    p: &mut BitMatrix,
    adjacency: &[BitMatrix],
    clique_size: usize,
    limit: i32,
) -> (i32, bool) {
    if depth == clique_size {
        // Found a clique - check if we've exceeded limit
        if limit <= 0 {
            return (1, true);
        }
        return (1, false);
    }

    let p_card = p.cardinality() as usize;

    // Leaf shortcut: with one slot left, each candidate in P completes exactly
    // one k-clique (P is the common neighborhood of everything committed).
    if depth == clique_size - 1 {
        let found = p_card as i32;
        return (found, found > limit);
    }

    // Subsumes the empty-P check (depth < clique_size - 1 here).
    if depth + p_card < clique_size {
        return (0, false);
    }

    // Second-to-last level inline: each child would immediately take the leaf
    // shortcut, so fold it in — one AND + popcount per candidate, no recursion.
    // The exceeded condition (count > limit) is exactly what the child's
    // `found > remaining` plus the parent's post-child check reduce to.
    if depth == clique_size - 2 {
        let mut count = 0;
        let candidates = *p;
        for v in candidates.iter_set_bits() {
            let mut pv = *p;
            pv.and_assign(&adjacency[v]);
            count += pv.cardinality() as i32;
            if count > limit {
                return (count, true);
            }
            p.clear(v);
        }
        return (count, false);
    }

    let mut count = 0;
    let candidates = *p;
    for v in candidates.iter_set_bits() {
        let mut new_p = *p;
        new_p.and_assign(&adjacency[v]);

        let remaining = limit - count;
        let (sub_count, exceeded) =
            bron_kerbosch_count_no_x_with_limit(depth + 1, &mut new_p, adjacency, clique_size, remaining);
        count += sub_count;

        if exceeded || count > limit {
            return (count, true);
        }

        p.clear(v);
    }

    (count, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;

    #[test]
    fn k5_has_exactly_one_monochromatic_5_clique() {
        let bits = "1".repeat(10);
        let mut g = Graph::from_bitstring(&bits, 5);
        let cliques = get_all_cliques(&mut g, 5);
        // K5 has 1 red 5-clique; complement has 0 5-cliques.
        assert_eq!(cliques.len(), 1);
        let count = get_cliques_comprehensive(&mut g, 5);
        assert_eq!(count, 1);
    }

    #[test]
    fn empty_5v_graph_has_one_blue_5_clique() {
        let bits = "0".repeat(10);
        let mut g = Graph::from_bitstring(&bits, 5);
        // No red edges, but the complement is K5 → 1 blue 5-clique.
        let cliques = get_all_cliques(&mut g, 5);
        assert_eq!(cliques.len(), 1);
        let count = get_cliques_comprehensive(&mut g, 5);
        assert_eq!(count, 1);
    }

    #[test]
    fn k4_has_zero_5_cliques_either_color() {
        // K4 (4 vertices, all 6 edges set). Cannot contain any 5-clique
        // because there are only 4 vertices.
        let bits = "1".repeat(6);
        let mut g = Graph::from_bitstring(&bits, 4);
        let cliques = get_all_cliques(&mut g, 5);
        assert_eq!(cliques.len(), 0);
        assert_eq!(get_cliques_comprehensive(&mut g, 5), 0);
    }

    #[test]
    fn get_all_cliques_matches_comprehensive_count_on_small_random_graphs() {
        // Each bitstring is exhaustively chosen for n=6 (15 edges → 15-bit strings).
        // Sample a handful of fixed seeds; both algorithms must agree on count.
        let cases = [
            "000000000000000",
            "111111111111111",
            "101010101010101",
            "110011001100110",
            "111000111000111",
            "010101010101010",
        ];
        for bits in cases {
            let mut g = Graph::from_bitstring(bits, 6);
            let by_collect = get_all_cliques(&mut g, 4).len() as i32;
            let by_count = get_cliques_comprehensive(&mut g, 4);
            assert_eq!(
                by_collect, by_count,
                "mismatch on bits={bits} for clique_size=4: collect={by_collect}, count={by_count}"
            );
        }
    }

    #[test]
    fn k8_has_56_red_5_cliques_plus_blue() {
        // K8: C(8,5) = 56 red 5-cliques, 0 blue — heavy leaf fan-out through the
        // with-X counting path; collect (no shortcut) is the reference.
        let bits = "1".repeat(28);
        let mut g = Graph::from_bitstring(&bits, 8);
        assert_eq!(get_all_cliques(&mut g, 5).len(), 56);
        assert_eq!(get_cliques_comprehensive(&mut g, 5), 56);
    }

    #[test]
    fn count_matches_collect_on_dense_7v_graphs_small_k() {
        // clique_size 3 puts almost every node at the leaf level; the collect
        // variant has no shortcut and cross-checks the counting variants.
        let cases = [
            "111111111111111111111", // K7
            "110111011101110111011",
            "101101101101101101101",
            "111100011110001111000",
        ];
        for bits in cases {
            for k in [3usize, 4] {
                let mut g = Graph::from_bitstring(bits, 7);
                let by_collect = get_all_cliques(&mut g, k).len() as i32;
                let by_count = get_cliques_comprehensive(&mut g, k);
                assert_eq!(by_collect, by_count, "bits={bits} k={k}");
            }
        }
    }

    #[test]
    fn limit_zero_reports_exceeded_when_cliques_form() {
        // K5 with edge (0,1) flipped away has no mono 5-clique; flipping it back
        // creates one. With limit 0 the seeded search must report exceeded.
        let mut bits: Vec<u8> = "1".repeat(10).into_bytes();
        bits[0] = b'0'; // edge (0,1) blue
        let mut g = Graph::from_bitstring(std::str::from_utf8(&bits).unwrap(), 5);
        let flip = [crate::graph::WorkUnitEdge { vertex_one: 0, vertex_two: 1 }];
        g.flip_edges(&flip); // now K5 again; new red clique contains (0,1)
        let (_, exceeded) = get_new_cliques_with_limit(&mut g, 5, &flip, 0);
        assert!(exceeded);
        let (count, exceeded) = get_new_cliques_with_limit(&mut g, 5, &flip, 10);
        assert!(!exceeded);
        assert_eq!(count, 1);
    }

    #[test]
    fn flipping_edge_changes_clique_count() {
        // K5 has 1 red 5-clique. Flip one edge → no monochromatic 5-clique
        // (4 red edges + 1 blue edge means neither color has a K5).
        let bits = "1".repeat(10);
        let mut g = Graph::from_bitstring(&bits, 5);
        assert_eq!(get_cliques_comprehensive(&mut g, 5), 1);
        g.flip_edges(&[crate::graph::WorkUnitEdge { vertex_one: 0, vertex_two: 1 }]);
        assert_eq!(get_cliques_comprehensive(&mut g, 5), 0);
    }
}
