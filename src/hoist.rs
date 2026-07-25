//! Hoisted evaluation of pair moves — computes the SAME `created` count the seeded
//! Bron-Kerbosch kernel computes, but with (almost always) no traversal at all.
//!
//! # Why this works
//!
//! A pair move flips red edge `r` and blue edge `b`, so in the derived graph
//! `R' = (R ∪ {b}) \ {r}` and `B' = (B ∪ {r}) \ {b}`. Only a newly-coloured edge can be in a NEW
//! clique, so
//!
//! ```text
//! created = (# red cliques through b in R')  +  (# blue cliques through r in B')
//!         = (C_b − X)                        +  (D_r − Y)
//! ```
//!
//! where `C_b` / `D_r` are properties of ONE edge and the base graph — exactly what flipping that
//! single edge alone would create — and the corrections are
//!
//! ```text
//! X = # red  cliques in R ∪ {b} containing BOTH r and b
//! Y = # blue cliques in B ∪ {r} containing BOTH r and b
//! ```
//!
//! `C_b` and `D_r` do not depend on the partner edge, so across a stage's ~392M pairs each is
//! needed ~19,810 times but computed once ([`HoistTables`] memoises them lazily).
//!
//! # The cheap test
//!
//! A clique containing both `r = (x,y)` and `b = (u,v)` must contain every vertex of both, so
//! every internal pair of that vertex set must share the colour. Two of those pairs are `r` and
//! `b` themselves (red-by-assumption for X, blue-by-assumption for Y); the rest are the **cross
//! pairs**. So `X > 0` requires all cross pairs RED and `Y > 0` requires all cross pairs BLUE.
//! There is always at least one cross pair, so **X and Y can never both be non-zero**, and when
//! the cross pairs are mixed BOTH are zero and `created = C_b + D_r` exactly.
//!
//! # Shared vertices (the subtle case)
//!
//! When `r` and `b` share a vertex there are only THREE forced vertices and exactly ONE cross
//! pair. Testing the four `{r-endpoint, b-endpoint}` products blindly is WRONG: one product is
//! degenerate (a vertex with itself, which reads as a self-loop) and another collapses onto `r`
//! or `b` itself. Both "all red" and "all blue" then come out false, every shared-vertex pair is
//! misclassified as mixed, and its correction is silently dropped. That is 1.4% of the move space
//! answered wrongly, so [`cross_pairs`] excludes those products explicitly.

use crate::algorithm::{count_cliques_through_vertex_set, get_new_cliques_with_limit};
use crate::bitset::BitMatrix;
use crate::graph::{Graph, WorkUnitEdge};

/// Sentinel for "not computed yet". Real `created` counts are always >= 0.
const UNKNOWN: i32 = i32::MIN;

/// Which colour the cross pairs share, which decides whether a correction is needed at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CrossPairs {
    /// Both corrections are provably zero — `created = C_b + D_r`, no traversal.
    Mixed,
    /// Only X can be non-zero (a red clique may contain both edges).
    AllRed,
    /// Only Y can be non-zero.
    AllBlue,
}

/// Classify the cross pairs of red edge `r = (x,y)` against blue edge `b = (u,v)`.
///
/// `red_adj` is the BASE graph's red adjacency (unflipped).
#[inline]
pub fn cross_pairs(red_adj: &[BitMatrix], r: (usize, usize), b: (usize, usize)) -> CrossPairs {
    let (x, y) = r;
    let (u, v) = b;
    let mut all_red = true;
    let mut all_blue = true;
    for a in [x, y] {
        for c in [u, v] {
            // Skip products that are not cross pairs: the degenerate one (shared vertex against
            // itself) and the two that collapse onto r or b when a vertex is shared. See the
            // module docs — getting this wrong silently corrupts every shared-vertex pair.
            if a == c
                || (a == x && c == y)
                || (a == y && c == x)
                || (a == u && c == v)
                || (a == v && c == u)
            {
                continue;
            }
            if red_adj[a].get(c) {
                all_blue = false;
            } else {
                all_red = false;
            }
        }
    }
    if all_red {
        CrossPairs::AllRed
    } else if all_blue {
        CrossPairs::AllBlue
    } else {
        CrossPairs::Mixed
    }
}

/// The distinct vertices a clique containing both `r` and `b` is forced to contain (3 or 4).
#[inline]
fn forced_vertices(r: (usize, usize), b: (usize, usize)) -> ([usize; 4], usize) {
    let mut s = [0usize; 4];
    let mut n = 0;
    for w in [r.0, r.1, b.0, b.1] {
        if !s[..n].contains(&w) {
            s[n] = w;
            n += 1;
        }
    }
    (s, n)
}

/// The correction term (X or Y): cliques of `adjacency`'s colour containing both edges.
///
/// **No graph mutation is needed.** X is defined over `R ∪ {b}`, but the only difference from `R`
/// is the edge `b`, whose two endpoints are both in the forced set and are therefore cleared out
/// of the candidate set. Every edge induced on the candidates is an original edge, so the base
/// adjacency is used unchanged. The same argument gives Y over `B ∪ {r}` in the complement.
#[inline]
fn correction(
    adjacency: &[BitMatrix],
    r: (usize, usize),
    b: (usize, usize),
    clique_size: usize,
) -> i32 {
    let (seeds, n) = forced_vertices(r, b);
    count_cliques_through_vertex_set(adjacency, &seeds[..n], clique_size)
}

/// Lazily-memoised per-edge `created` counts for ONE base graph.
///
/// Filled on demand rather than precomputed, which matters for more than convenience:
/// * In the near-floor sweep every entry is wanted, and filling one costs about what the
///   traversal it replaces costs — so the first pass over an edge is free and the other ~19,809
///   uses of it are lookups.
/// * Mid-descent the bound-skip rejects most units from the per-edge counts alone, before any
///   value here is needed, so almost nothing is computed and a stage that advances after a
///   fraction of a sweep has paid nothing.
///
/// A precomputed table would instead cost a fixed ~6 s per stage, which is fine during a wall but
/// ruinous during a descent where stages advance in about a second.
#[derive(Debug, Clone)]
pub struct HoistTables {
    vertex_count: usize,
    /// `created` for flipping that ONE edge, indexed `min * vertex_count + max`. For a blue edge
    /// this is `C_b`, for a red edge `D_r` — an edge has exactly one colour, so one table serves
    /// both. Doubles as the answer for single-flip work units.
    single: Vec<i32>,
}

impl HoistTables {
    pub fn new(vertex_count: usize) -> Self {
        HoistTables {
            vertex_count,
            single: vec![UNKNOWN; vertex_count * vertex_count],
        }
    }

    #[inline]
    fn index(&self, u: usize, v: usize) -> usize {
        let (a, b) = if u < v { (u, v) } else { (v, u) };
        a * self.vertex_count + b
    }

    /// How many entries have been computed — for logging/tests only.
    pub fn filled(&self) -> usize {
        self.single.iter().filter(|&&x| x != UNKNOWN).count()
    }

    /// `created` for flipping the single edge `(u,v)` of the base graph, memoised.
    ///
    /// `graph` must be the BASE graph; it is flipped and restored, so it is unchanged on return.
    pub fn single_created(
        &mut self,
        graph: &mut Graph,
        clique_size: usize,
        u: usize,
        v: usize,
    ) -> i32 {
        let i = self.index(u, v);
        let cached = self.single[i];
        if cached != UNKNOWN {
            return cached;
        }
        let edge = [WorkUnitEdge {
            vertex_one: u as u16,
            vertex_two: v as u16,
        }];
        graph.flip_edges(&edge);
        // Uncapped: the value is reused across ~19,810 partners with different abort limits, so a
        // capped entry could be reused where the true value was needed. (Capping also saves
        // nothing measurable — the cost is neighbourhood exploration, not clique counting.)
        let (created, _) = get_new_cliques_with_limit(graph, clique_size, &edge, i32::MAX);
        graph.flip_edges(&edge);
        self.single[i] = created;
        created
    }

    /// `created` for the pair move that flips red edge `r` and blue edge `b`. EXACT — identical to
    /// what the seeded kernel returns for the same move on the same base graph.
    ///
    /// `graph` must be the BASE graph and is unchanged on return.
    pub fn pair_created(
        &mut self,
        graph: &mut Graph,
        clique_size: usize,
        r: (usize, usize),
        b: (usize, usize),
    ) -> i32 {
        let base = self.single_created(graph, clique_size, b.0, b.1)
            + self.single_created(graph, clique_size, r.0, r.1);
        match cross_pairs(&graph.adjacency, r, b) {
            CrossPairs::Mixed => base,
            CrossPairs::AllRed => base - correction(&graph.adjacency, r, b, clique_size),
            CrossPairs::AllBlue => {
                base - correction(&graph.complement_adjacency, r, b, clique_size)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algorithm::get_new_cliques_with_limit;

    /// What the production kernel returns for this move — the reference for every test here.
    fn kernel_created(graph: &mut Graph, k: usize, edges: &[WorkUnitEdge]) -> i32 {
        graph.flip_edges(edges);
        let (created, _) = get_new_cliques_with_limit(graph, k, edges, i32::MAX);
        graph.flip_edges(edges);
        created
    }

    fn edge(u: usize, v: usize) -> WorkUnitEdge {
        WorkUnitEdge {
            vertex_one: u as u16,
            vertex_two: v as u16,
        }
    }

    /// Deterministic pseudo-random bitstring so the fixtures are reproducible.
    fn bits(n: usize, seed: u64) -> String {
        let mut s = seed;
        let mut out = String::new();
        for _ in 0..(n * (n - 1) / 2) {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            out.push(if (s >> 60) & 1 == 1 { '1' } else { '0' });
        }
        out
    }

    /// Every pair move on a small graph, both colours, disjoint AND shared-vertex, against the
    /// production kernel. This is the test that fails if the cross-pair classification is wrong.
    #[test]
    fn pair_created_matches_kernel_on_every_pair_of_small_graphs() {
        for (n, k, seed) in [(9usize, 4usize, 7u64), (10, 4, 11), (10, 5, 3), (12, 5, 29)] {
            let b = bits(n, seed);
            let mut graph = Graph::from_bitstring(&b, n);
            let mut reds = Vec::new();
            let mut blues = Vec::new();
            for i in 0..n {
                for j in (i + 1)..n {
                    if graph.adjacency[i].get(j) {
                        reds.push((i, j))
                    } else {
                        blues.push((i, j))
                    }
                }
            }
            let mut tables = HoistTables::new(n);
            let mut shared_seen = 0;
            for &r in &reds {
                for &bl in &blues {
                    let expected = kernel_created(&mut graph, k, &[edge(r.0, r.1), edge(bl.0, bl.1)]);
                    let got = tables.pair_created(&mut graph, k, r, bl);
                    assert_eq!(
                        expected, got,
                        "n={n} k={k} seed={seed} r={r:?} b={bl:?} class={:?}",
                        cross_pairs(&graph.adjacency, r, bl)
                    );
                    if r.0 == bl.0 || r.0 == bl.1 || r.1 == bl.0 || r.1 == bl.1 {
                        shared_seen += 1;
                    }
                }
            }
            assert!(shared_seen > 0, "fixture n={n} exercised no shared-vertex pairs");
        }
    }

    /// The single-flip table entry must equal what the kernel computes for that same flip, in
    /// both colours — this is what makes single-flip work units a pure lookup.
    #[test]
    fn single_created_matches_kernel_for_both_colours() {
        let n = 11;
        let k = 4;
        let mut graph = Graph::from_bitstring(&bits(n, 5), n);
        let mut tables = HoistTables::new(n);
        for i in 0..n {
            for j in (i + 1)..n {
                let expected = kernel_created(&mut graph, k, &[edge(i, j)]);
                assert_eq!(expected, tables.single_created(&mut graph, k, i, j), "edge ({i},{j})");
            }
        }
        assert_eq!(tables.filled(), n * (n - 1) / 2);
    }

    /// The claim the fast path rests on: cross pairs mixed ⇒ both corrections vanish, so
    /// `created` is exactly the sum of the two single-flip values.
    #[test]
    fn mixed_cross_pairs_need_no_correction() {
        let n = 12;
        let k = 5;
        let mut graph = Graph::from_bitstring(&bits(n, 29), n);
        let mut tables = HoistTables::new(n);
        let mut checked = 0;
        for i in 0..n {
            for j in (i + 1)..n {
                if !graph.adjacency[i].get(j) {
                    continue;
                }
                for a in 0..n {
                    for c in (a + 1)..n {
                        if graph.adjacency[a].get(c) {
                            continue;
                        }
                        let (r, b) = ((i, j), (a, c));
                        if cross_pairs(&graph.adjacency, r, b) != CrossPairs::Mixed {
                            continue;
                        }
                        let sum = tables.single_created(&mut graph, k, b.0, b.1)
                            + tables.single_created(&mut graph, k, r.0, r.1);
                        assert_eq!(sum, kernel_created(&mut graph, k, &[edge(i, j), edge(a, c)]));
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 0);
    }

    /// A shared vertex leaves exactly ONE cross pair, so such a pair is never mixed. The buggy
    /// four-product test reports Mixed for all of them, which this pins down.
    #[test]
    fn shared_vertex_pairs_are_never_mixed() {
        let n = 12;
        let mut graph = Graph::from_bitstring(&bits(n, 13), n);
        graph.resync_complement();
        let mut seen = 0;
        for i in 0..n {
            for j in (i + 1)..n {
                if !graph.adjacency[i].get(j) {
                    continue;
                }
                for a in 0..n {
                    for c in (a + 1)..n {
                        if graph.adjacency[a].get(c) {
                            continue;
                        }
                        let (r, b) = ((i, j), (a, c));
                        let shares = r.0 == b.0 || r.0 == b.1 || r.1 == b.0 || r.1 == b.1;
                        if !shares {
                            continue;
                        }
                        assert_ne!(
                            cross_pairs(&graph.adjacency, r, b),
                            CrossPairs::Mixed,
                            "shared-vertex pair r={r:?} b={b:?} classified Mixed"
                        );
                        seen += 1;
                    }
                }
            }
        }
        assert!(seen > 0, "fixture exercised no shared-vertex pairs");
    }

    /// Memoisation must not change any answer, only the cost.
    #[test]
    fn repeated_queries_are_stable_and_leave_the_graph_untouched() {
        let n = 10;
        let k = 4;
        let before = bits(n, 17);
        let mut graph = Graph::from_bitstring(&before, n);
        let mut tables = HoistTables::new(n);
        let r = (0..n)
            .flat_map(|i| ((i + 1)..n).map(move |j| (i, j)))
            .find(|&(i, j)| graph.adjacency[i].get(j))
            .unwrap();
        let b = (0..n)
            .flat_map(|i| ((i + 1)..n).map(move |j| (i, j)))
            .find(|&(i, j)| !graph.adjacency[i].get(j))
            .unwrap();
        let first = tables.pair_created(&mut graph, k, r, b);
        for _ in 0..5 {
            assert_eq!(first, tables.pair_created(&mut graph, k, r, b));
        }
        assert_eq!(graph.to_bitstring(), before, "base graph must be restored");
    }
}
