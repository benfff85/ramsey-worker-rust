//! Shared helpers for the regression test suite.
//!
//! Everything here is deliberately independent of the production algorithms:
//! the brute-force counter iterates raw vertex subsets, the PRNG is a fixed
//! inline LCG (stable across platforms and dependency upgrades), and the Paley
//! generator builds graphs whose clique structure is known mathematically.

#![allow(dead_code)] // each integration-test binary uses a subset of these helpers

use ramsey_worker_rust::graph::Graph;

/// Minimal deterministic PRNG (Knuth/Numerical Recipes LCG). Used instead of
/// the `rand` crate so fixture graphs never change across dependency bumps.
pub struct Lcg(u64);

impl Lcg {
    pub fn new(seed: u64) -> Self {
        Lcg(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }

    pub fn next_bool(&mut self) -> bool {
        (self.next_u64() >> 33) & 1 == 1
    }
}

/// Deterministic random bitstring for an n-vertex graph fixture.
pub fn random_bitstring(seed: u64, vertex_count: usize) -> String {
    let len = vertex_count * (vertex_count - 1) / 2;
    let mut lcg = Lcg::new(seed);
    (0..len)
        .map(|_| if lcg.next_bool() { '1' } else { '0' })
        .collect()
}

/// Independent reference implementation: count monochromatic k-cliques by
/// enumerating every k-subset of vertices and checking that all pairs share
/// one color. Exponential — only for small n — but shares no code with the
/// Bron-Kerbosch implementation under test.
pub fn brute_force_mono_clique_count(graph: &Graph, clique_size: usize) -> i32 {
    fn is_monochromatic(graph: &Graph, subset: &[usize]) -> bool {
        let first = graph.adjacency[subset[0]].get(subset[1]);
        for i in 0..subset.len() {
            for j in (i + 1)..subset.len() {
                if graph.adjacency[subset[i]].get(subset[j]) != first {
                    return false;
                }
            }
        }
        true
    }

    fn recurse(
        graph: &Graph,
        clique_size: usize,
        start: usize,
        current: &mut Vec<usize>,
        count: &mut i32,
    ) {
        if current.len() == clique_size {
            if is_monochromatic(graph, current) {
                *count += 1;
            }
            return;
        }
        for v in start..graph.vertex_count {
            current.push(v);
            recurse(graph, clique_size, v + 1, current, count);
            current.pop();
        }
    }

    let mut count = 0;
    recurse(graph, clique_size, 0, &mut Vec::new(), &mut count);
    count
}

/// Paley graph bitstring on p vertices (p prime, p ≡ 1 mod 4): edge (i, j) is
/// red iff (j - i) is a quadratic residue mod p. The p ≡ 1 (mod 4) condition
/// makes the residue set symmetric, so the graph is well-defined undirected
/// and self-complementary. Clique structure is known from the literature,
/// giving ground-truth anchors that owe nothing to the code under test.
pub fn paley_bitstring(p: usize) -> String {
    let mut is_qr = vec![false; p];
    for x in 1..p {
        is_qr[(x * x) % p] = true;
    }
    let mut bits = String::with_capacity(p * (p - 1) / 2);
    for i in 0..p {
        for j in (i + 1)..p {
            bits.push(if is_qr[(j - i) % p] { '1' } else { '0' });
        }
    }
    bits
}

/// Number of monochromatic triangles in the Paley 2-coloring of K_p, from the
/// strongly-regular parameters (p, (p-1)/2, (p-5)/4, (p-1)/4): each color class
/// contains p(p-1)(p-5)/48 triangles, and the coloring is self-complementary.
pub fn paley_mono_triangle_count(p: usize) -> i32 {
    (2 * p * (p - 1) * (p - 5) / 48) as i32
}

/// Red (color '1') edge count of a bitstring.
pub fn red_count(bits: &str) -> usize {
    bits.chars().filter(|&c| c == '1').count()
}
