use crate::bitset::BitMatrix;
use crate::graph::{Graph, WorkUnitEdge};

// Replicates TargetedCliqueCheckServiceBitSet.getNewCliques
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
            let mut r = BitMatrix::new(graph.vertex_count);
            r.set(v1);
            r.set(v2);

            let mut p = graph.adjacency[v1].clone();
            p.and_assign(&graph.adjacency[v2]);
            p.clear(v1);
            p.clear(v2);

            let x = BitMatrix::new(graph.vertex_count);
            new_clique_count += bron_kerbosch_count(r, p, x, &graph.adjacency, clique_size);
        }
    }

    // Check BLUE (inverted adjacency)
    graph.invert();
    for edge in flipped_edges {
        let v1 = edge.vertex_one as usize;
        let v2 = edge.vertex_two as usize;

        if graph.adjacency[v1].get(v2) {
            let mut r = BitMatrix::new(graph.vertex_count);
            r.set(v1);
            r.set(v2);

            let mut p = graph.adjacency[v1].clone();
            p.and_assign(&graph.adjacency[v2]);
            p.clear(v1);
            p.clear(v2);

            let x = BitMatrix::new(graph.vertex_count);
            new_clique_count += bron_kerbosch_count(r, p, x, &graph.adjacency, clique_size);
        }
    }

    // Restore graph state (optional, but good practice if graph is reused)
    graph.invert();

    new_clique_count
}

pub fn get_all_cliques(graph: &mut Graph, clique_size: usize) -> Vec<Vec<usize>> {
    let mut cliques = Vec::new();

    // RED
    let r = BitMatrix::new(graph.vertex_count);
    let mut p = BitMatrix::new(graph.vertex_count);
    let x = BitMatrix::new(graph.vertex_count);

    // Set all bits in P to 1
    for i in 0..graph.vertex_count {
        p.set(i);
    }

    bron_kerbosch_collect(
        r.clone(),
        p.clone(),
        x.clone(),
        &graph.adjacency,
        clique_size,
        &mut cliques,
    );

    // BLUE
    graph.invert();

    // Reset for Blue pass
    // R is empty, P is full, X is empty
    let r_blue = BitMatrix::new(graph.vertex_count);
    let mut p_blue = BitMatrix::new(graph.vertex_count);
    let x_blue = BitMatrix::new(graph.vertex_count);
    for i in 0..graph.vertex_count {
        p_blue.set(i);
    }

    bron_kerbosch_collect(
        r_blue,
        p_blue,
        x_blue,
        &graph.adjacency,
        clique_size,
        &mut cliques,
    );

    graph.invert(); // Restore

    cliques
}

fn bron_kerbosch_collect(
    mut r: BitMatrix,
    mut p: BitMatrix,
    mut x: BitMatrix,
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

    let candidates = p.clone();

    let mut v_opt = candidates.next_set_bit(0);
    while let Some(v) = v_opt {
        r.set(v);

        let mut new_p = p.clone();
        new_p.and_assign(&adjacency[v]);

        bron_kerbosch_collect(r.clone(), new_p, x.clone(), adjacency, clique_size, cliques);

        r.clear(v);
        p.clear(v);
        x.set(v);

        v_opt = candidates.next_set_bit(v + 1);
    }
}

pub fn get_cliques_comprehensive(graph: &mut Graph, clique_size: usize) -> i32 {
    let mut clique_count = 0;

    // RED
    let r = BitMatrix::new(graph.vertex_count); // removed mut
    let mut p = BitMatrix::new(graph.vertex_count);
    let x = BitMatrix::new(graph.vertex_count);

    // Set all bits in P to 1 (all vertices are candidates initially)
    for i in 0..graph.vertex_count {
        p.set(i);
    }

    clique_count += bron_kerbosch_count(
        r.clone(),
        p.clone(),
        x.clone(),
        &graph.adjacency,
        clique_size,
    );

    // BLUE
    graph.invert();
    clique_count += bron_kerbosch_count(r, p, x, &graph.adjacency, clique_size);
    graph.invert(); // Restore

    clique_count
}

fn bron_kerbosch_count(
    mut r: BitMatrix,
    mut p: BitMatrix, // reverted to mut
    mut x: BitMatrix,
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
    let candidates = p.clone();

    let mut v_opt = candidates.next_set_bit(0);
    while let Some(v) = v_opt {
        r.set(v);

        let mut new_p = p.clone();
        new_p.and_assign(&adjacency[v]);

        count += bron_kerbosch_count(r.clone(), new_p, x.clone(), adjacency, clique_size);

        r.clear(v);
        p.clear(v);
        x.set(v);

        v_opt = candidates.next_set_bit(v + 1);
    }

    count
}
