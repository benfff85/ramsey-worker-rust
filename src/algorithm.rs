use crate::bitset::BitMatrix;
use crate::graph::{Graph, WorkUnitEdge};

/// Replicates TargetedCliqueCheckServiceBitSet.getNewCliques
/// Counts new cliques formed after edge flips, using seeded Bron-Kerbosch.
pub fn get_new_cliques(
    graph: &mut Graph,
    clique_size: usize,
    flipped_edges: &[WorkUnitEdge],
) -> i32 {
    let mut new_clique_count = 0;

    // Check RED (current adjacency)
    for edge in flipped_edges {
        let v1 = edge.vertex_one as usize;
        let v2 = edge.vertex_two as usize;

        if graph.adjacency[v1].get(v2) {
            let mut r = BitMatrix::new();
            r.set(v1);
            r.set(v2);

            let mut p = graph.adjacency[v1];
            p.and_assign(&graph.adjacency[v2]);
            p.clear(v1);
            p.clear(v2);

            let mut x = BitMatrix::new();
            new_clique_count +=
                bron_kerbosch_count_inplace(&mut r, &mut p, &mut x, &graph.adjacency, clique_size);
        }
    }

    // Check BLUE (inverted adjacency)
    graph.invert();
    for edge in flipped_edges {
        let v1 = edge.vertex_one as usize;
        let v2 = edge.vertex_two as usize;

        if graph.adjacency[v1].get(v2) {
            let mut r = BitMatrix::new();
            r.set(v1);
            r.set(v2);

            let mut p = graph.adjacency[v1];
            p.and_assign(&graph.adjacency[v2]);
            p.clear(v1);
            p.clear(v2);

            let mut x = BitMatrix::new();
            new_clique_count +=
                bron_kerbosch_count_inplace(&mut r, &mut p, &mut x, &graph.adjacency, clique_size);
        }
    }

    // Restore graph state
    graph.invert();

    new_clique_count
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
/// This avoids cloning R and X on every recursive call.
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

    // Copy candidates to iterate over (Copy is free with fixed array)
    let candidates = *p;

    let mut v_opt = candidates.next_set_bit(0);
    while let Some(v) = v_opt {
        // Add v to R
        r.set(v);

        // Compute new_p = p ∩ N(v)
        let mut new_p = *p;
        new_p.and_assign(&adjacency[v]);

        // Compute new_x = x ∩ N(v)
        let mut new_x = *x;
        new_x.and_assign(&adjacency[v]);

        // Recurse with modified sets
        bron_kerbosch_collect_inplace(r, &mut new_p, &mut new_x, adjacency, clique_size, cliques);

        // Backtrack: remove v from R
        r.clear(v);

        // Move v from P to X
        p.clear(v);
        x.set(v);

        v_opt = candidates.next_set_bit(v + 1);
    }
}

pub fn get_cliques_comprehensive(graph: &mut Graph, clique_size: usize) -> i32 {
    let mut clique_count = 0;

    // RED
    let mut r = BitMatrix::new();
    let mut p = BitMatrix::new();
    let mut x = BitMatrix::new();

    // Set all bits in P to 1 (all vertices are candidates initially)
    for i in 0..graph.vertex_count {
        p.set(i);
    }

    clique_count +=
        bron_kerbosch_count_inplace(&mut r, &mut p, &mut x, &graph.adjacency, clique_size);

    // BLUE
    graph.invert();

    // Reset for blue pass
    let mut r_blue = BitMatrix::new();
    let mut p_blue = BitMatrix::new();
    let mut x_blue = BitMatrix::new();
    for i in 0..graph.vertex_count {
        p_blue.set(i);
    }

    clique_count += bron_kerbosch_count_inplace(
        &mut r_blue,
        &mut p_blue,
        &mut x_blue,
        &graph.adjacency,
        clique_size,
    );
    graph.invert(); // Restore

    clique_count
}

/// Bron-Kerbosch counting variant with in-place mutation and backtracking.
/// This matches the Java implementation's approach of mutating R in place.
#[inline]
fn bron_kerbosch_count_inplace(
    r: &mut BitMatrix,
    p: &mut BitMatrix,
    x: &mut BitMatrix,
    adjacency: &[BitMatrix],
    clique_size: usize,
) -> i32 {
    if r.cardinality() as usize == clique_size {
        return 1;
    }

    if ((r.cardinality() + p.cardinality()) as usize) < clique_size {
        return 0;
    }

    if p.is_empty() {
        return 0;
    }

    let mut count = 0;

    // Copy candidates to iterate over (Copy is free with fixed array)
    let candidates = *p;

    let mut v_opt = candidates.next_set_bit(0);
    while let Some(v) = v_opt {
        // Add v to R
        r.set(v);

        // Compute new_p = p ∩ N(v)
        let mut new_p = *p;
        new_p.and_assign(&adjacency[v]);

        // Compute new_x = x ∩ N(v)
        let mut new_x = *x;
        new_x.and_assign(&adjacency[v]);

        // Recurse
        count += bron_kerbosch_count_inplace(r, &mut new_p, &mut new_x, adjacency, clique_size);

        // Backtrack: remove v from R
        r.clear(v);

        // Move v from P to X
        p.clear(v);
        x.set(v);

        v_opt = candidates.next_set_bit(v + 1);
    }

    count
}
