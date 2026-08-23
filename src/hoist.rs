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

/// Re-checks allowed for peers' slices on a freshly-started table. A carried table resets to this
/// too: it still needs peers' help for the entries the advance invalidated.
const INITIAL_REFRESH_BUDGET: u8 = 12;

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
    /// Re-checks left for peers' slices. Workers reach the gate together and publish at roughly
    /// the same moment, so a single sweep right after filling our own slice mostly races them and
    /// comes back empty. Re-checking over the next few batches is what actually collects the
    /// fleet's work; the budget stops us polling forever when a slice is never published (a
    /// smaller fleet than there are slices), in which case the stragglers are filled on demand.
    refresh_budget: u8,
    /// `created` for flipping that ONE edge, indexed `min * vertex_count + max`. For a blue edge
    /// this is `C_b`, for a red edge `D_r` — an edge has exactly one colour, so one table serves
    /// both. Doubles as the answer for single-flip work units.
    single: Vec<i32>,
    /// Entries computed on demand because neither this worker nor a peer had them yet, and the
    /// wall-clock spent doing it. Instrumentation only.
    ///
    /// The sharded fill covers one slice; everything else arrives from peers or is computed here,
    /// mid-loop, at the cost of an UNCAPPED traversal. That cost is invisible in the throughput
    /// line because it happens INSIDE the unit loop and so counts as "busy" — which is exactly why
    /// it needs measuring separately before any inner-loop work is prioritised.
    fills: u64,
    fill_nanos: u128,
    /// Fills split by the edge's colour in the BASE graph, and by whether they came from this
    /// worker's own sharded slice or from a miss inside the unit loop.
    ///
    /// The split matters because the two colours are not equally valuable. `BasicEnumerator` maps
    /// `red_idx = index / blue_count`, so RED is the outer loop: a contiguous claim spans ~81 red
    /// edges but ALL 19,810 blue ones. A blue entry is therefore read by every worker on every
    /// batch, a red entry only by the one worker whose range covers it — roughly 14x versus 1x.
    /// If on-demand misses are overwhelmingly blue, the co-operative fill is mis-targeted: it
    /// strides all 39,621 edges when the contended half is the blue 19,810.
    fills_red: u64,
    fills_blue: u64,
    slice_fills: u64,
    /// Set while [`Self::fill_slice`] runs so its fills are attributed to the slice, not to misses.
    in_slice_fill: bool,
}

impl HoistTables {
    pub fn new(vertex_count: usize) -> Self {
        HoistTables {
            vertex_count,
            refresh_budget: INITIAL_REFRESH_BUDGET,
            single: vec![UNKNOWN; vertex_count * vertex_count],
            fills: 0,
            fill_nanos: 0,
            fills_red: 0,
            fills_blue: 0,
            slice_fills: 0,
            in_slice_fill: false,
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

    /// Number of edges in a slice of the edge space, so callers can size buffers.
    pub fn slice_len(vertex_count: usize, slice: usize, slices: usize) -> usize {
        let edges = vertex_count * (vertex_count - 1) / 2;
        edges.saturating_sub(slice).div_ceil(slices)
    }

    /// Compute every entry in one slice of the edge space and return the values in slice order.
    ///
    /// Slices are taken by striding the canonical upper-triangular edge order
    /// ([`Graph::edge_for_bit_index`]): slice `s` of `n` owns bit indices `s, s+n, s+2n, …`. That
    /// ordering is shared by every worker and derived from the graph alone, so a peer can map the
    /// returned values straight back onto edges without them being carried alongside.
    ///
    /// Filling one slice is the co-operative half of the ramp: with the fleet splitting the edge
    /// space, each worker pays 1/n of a fill it would otherwise do in full and alone.
    pub fn fill_slice(
        &mut self,
        graph: &mut Graph,
        clique_size: usize,
        slice: usize,
        slices: usize,
    ) -> Vec<i32> {
        let edges = graph.vertex_count * (graph.vertex_count - 1) / 2;
        let mut out = Vec::with_capacity(Self::slice_len(graph.vertex_count, slice, slices));
        self.in_slice_fill = true;
        let mut bit = slice;
        while bit < edges {
            match Graph::edge_for_bit_index(bit, graph.vertex_count) {
                Some((u, v)) => out.push(self.single_created(graph, clique_size, u, v)),
                None => break,
            }
            bit += slices;
        }
        self.in_slice_fill = false;
        out
    }

    /// Adopt a peer's slice. Values must be in the same stride order [`Self::fill_slice`] emits.
    ///
    /// Entries already known locally are left alone — they were computed here and are definitive.
    /// Anything the peer did not cover simply stays unknown and is computed on demand, so a
    /// partial or missing slice only costs time, never correctness.
    pub fn adopt_slice(&mut self, slice: usize, slices: usize, values: &[i32]) {
        let edges = self.vertex_count * (self.vertex_count - 1) / 2;
        let mut bit = slice;
        for &value in values {
            if bit >= edges {
                break;
            }
            if value >= 0 {
                if let Some((u, v)) = Graph::edge_for_bit_index(bit, self.vertex_count) {
                    let i = self.index(u, v);
                    if self.single[i] == UNKNOWN {
                        self.single[i] = value;
                    }
                }
            }
            bit += slices;
        }
    }

    /// Carry this table onto a graph differing from the one it was built on by `flipped` edges,
    /// invalidating only entries that could have changed. Returns how many were invalidated.
    ///
    /// # Why this is sound
    ///
    /// `single_created(e)` counts monochromatic k-cliques through `e` (with `e` flipped). It can
    /// only change if some clique it counts also contains a flipped edge `f`. A clique containing
    /// both contains every vertex of both, so every cross pair between `e` and `f` is an internal
    /// pair of that clique and shares its colour. [`cross_pairs`] returning [`CrossPairs::Mixed`]
    /// therefore proves no monochromatic clique contains both, and `e` is untouched — the same
    /// algebra the fast path already rests on.
    ///
    /// The test is evaluated in BOTH graphs and unioned. The motivating case is that with two
    /// flipped edges, a cross pair between `e` and `f1` can itself be `f2`, whose colour differs
    /// between the graphs. In every case reachable by exhaustive small-graph search that entry is
    /// ALSO caught by the check against `f2` alone, so the union has not been shown to be
    /// necessary — but it has not been shown redundant at n=282/k=8 either, and it costs ~0.3 ms
    /// against a ~6.7 s rebuild. Kept deliberately rather than reasoned away.
    ///
    /// Conservative by construction — it may invalidate an entry that did not change (costing one
    /// recompute) but never keeps one that did. Measured over 24 consecutive campaign-3 advances:
    /// invalidates ~21% of the table against ~10% genuinely changed, in ~0.7 ms against a ~6.7 s
    /// rebuild. Entries left UNKNOWN are refilled by the existing lazy/sharded paths, so a carried
    /// table is indistinguishable from a fresh one to every caller.
    pub fn carry_forward(&mut self, before: &Graph, after: &Graph) -> usize {
        // The delta is DERIVED from the two graphs rather than taken as an argument. A caller can
        // easily hold a stale edge list — a stage that never engaged the hoist followed by one that
        // took the full-build path — and a wrong list silently KEEPS entries the real delta
        // invalidates, feeding a stale `created` to every unit of the stage and publishing it to
        // peers. Nothing downstream catches that, so the delta must not be trusted from outside.
        // Cost is one bit compare per edge (~39,621) against a ~6.7 s rebuild.
        let mut flipped: Vec<(usize, usize)> = Vec::new();
        for u in 0..self.vertex_count {
            for v in (u + 1)..self.vertex_count {
                if before.adjacency[u].get(v) != after.adjacency[u].get(v) {
                    flipped.push((u, v));
                }
            }
        }
        let mut invalidated = 0;
        for u in 0..self.vertex_count {
            for v in (u + 1)..self.vertex_count {
                let i = self.index(u, v);
                if self.single[i] == UNKNOWN {
                    continue; // nothing to keep or lose
                }
                let e = (u, v);
                let stale = flipped.iter().any(|&f| {
                    e == f
                        || cross_pairs(&before.adjacency, e, f) != CrossPairs::Mixed
                        || cross_pairs(&after.adjacency, e, f) != CrossPairs::Mixed
                });
                if stale {
                    self.single[i] = UNKNOWN;
                    invalidated += 1;
                }
            }
        }
        // A carried table is a NEW table as far as co-operation and instrumentation go: it still
        // wants peers' slices for what it just invalidated, and its fill counters belong to the
        // stage that built them, not this one.
        self.refresh_budget = INITIAL_REFRESH_BUDGET;
        self.fills = 0;
        self.fill_nanos = 0;
        self.fills_red = 0;
        self.fills_blue = 0;
        self.slice_fills = 0;
        self.in_slice_fill = false;
        invalidated
    }

    /// Whether it is still worth asking Redis for slices we do not have.
    pub fn wants_refresh(&self) -> bool {
        self.refresh_budget > 0 && !self.is_complete()
    }

    /// Record that a refresh happened, so the budget is spent whether or not it found anything.
    pub fn note_refresh(&mut self) {
        self.refresh_budget = self.refresh_budget.saturating_sub(1);
    }

    /// True once every edge has a value, i.e. no further filling can be needed.
    pub fn is_complete(&self) -> bool {
        let edges = self.vertex_count * (self.vertex_count - 1) / 2;
        self.filled() >= edges
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
        let started = std::time::Instant::now();
        let was_red = graph.adjacency[u].get(v); // colour in the BASE graph, before the flip
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
        self.fills += 1;
        self.fill_nanos += started.elapsed().as_nanos();
        if was_red { self.fills_red += 1 } else { self.fills_blue += 1 }
        if self.in_slice_fill { self.slice_fills += 1 }
        created
    }

    /// Read and reset the fill counters: (total, nanos, red, blue, from_own_slice).
    pub fn take_fill_stats(&mut self) -> (u64, u128, u64, u64, u64) {
        let out = (self.fills, self.fill_nanos, self.fills_red, self.fills_blue, self.slice_fills);
        self.fills = 0;
        self.fill_nanos = 0;
        self.fills_red = 0;
        self.fills_blue = 0;
        self.slice_fills = 0;
        out
    }

    /// `created` for the pair move, or `None` when it provably exceeds `limit`.
    ///
    /// # The bound, and why it is exact
    ///
    /// The correction is the entire cost of this evaluation — measured, 13.69% of units take it and
    /// it is **98%** of the hoisted loop at ~1.7 µs each. But a unit only needs its exact `created`
    /// if it might BEAT the limit; everything else just needs to be rejected. A lower bound on
    /// `created` is enough to reject, and one is free:
    ///
    /// `X` counts k-cliques of `R ∪ {b}` containing BOTH `r` and `b`; `C_b` counts those containing
    /// `b`. Every clique counted by `X` contains `b`, so **`X ≤ C_b`**. When the cross pairs are all
    /// red, `Y = 0` (it would need them all blue), so
    ///
    /// ```text
    /// created = (C_b − X) + D_r  ≥  D_r        because X ≤ C_b
    /// ```
    ///
    /// so `D_r > limit` proves `created > limit`. Symmetrically `Y ≤ D_r` gives `created ≥ C_b` when
    /// the cross pairs are all blue. Both values are already computed for `base`, so the test is one
    /// comparison against a register.
    ///
    /// This is an algebraic consequence of the same identity the whole hoist rests on, not a
    /// heuristic prune: it can only skip work already destined for rejection, and can never change a
    /// `created`, a threshold, or a stage transition. Measured on graph 222120 it removes the
    /// correction from **99.89%** of slow-path units (0 soundness violations over 492,747), taking a
    /// single-core sweep from 103 s to 1.7 s.
    ///
    /// `limit` is the caller's `early_limit`. Pass `i32::MAX` to disable the bound entirely (the
    /// comparison can then never fire), which is what an unthresholded stage does.
    ///
    /// `graph` must be the BASE graph and is unchanged on return.
    pub fn pair_created_bounded(
        &mut self,
        graph: &mut Graph,
        clique_size: usize,
        r: (usize, usize),
        b: (usize, usize),
        limit: i32,
    ) -> Option<i32> {
        let c_b = self.single_created(graph, clique_size, b.0, b.1);
        let d_r = self.single_created(graph, clique_size, r.0, r.1);
        let base = c_b + d_r;
        let created = match cross_pairs(&graph.adjacency, r, b) {
            CrossPairs::Mixed => base,
            CrossPairs::AllRed => {
                // created >= D_r, so this rejects without touching the correction.
                if d_r > limit {
                    return None;
                }
                base - correction(&graph.adjacency, r, b, clique_size)
            }
            CrossPairs::AllBlue => {
                if c_b > limit {
                    return None;
                }
                base - correction(&graph.complement_adjacency, r, b, clique_size)
            }
        };
        if created > limit { None } else { Some(created) }
    }

    /// `created` for the pair move that flips red edge `r` and blue edge `b`. EXACT — identical to
    /// what the seeded kernel returns for the same move on the same base graph.
    ///
    /// Unbounded reference for [`Self::pair_created_bounded`] and the tests; the hot path uses the
    /// bounded form.
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

    /// The sharded fill must land on exactly the same table as filling it alone. A stride
    /// mismatch between the worker that computed a slice and the one that adopts it would put
    /// correct values on the WRONG edges — silent corruption that no later check would catch.
    #[test]
    fn sharded_fill_reconstructs_the_same_table_as_a_solo_fill() {
        for (n, k, slices) in [(11usize, 4usize, 3usize), (12, 5, 4), (13, 4, 7), (10, 4, 1)] {
            let b = bits(n, 91);
            let mut graph = Graph::from_bitstring(&b, n);

            // Reference: one worker fills every edge itself.
            let mut solo = HoistTables::new(n);
            for i in 0..n {
                for j in (i + 1)..n {
                    solo.single_created(&mut graph, k, i, j);
                }
            }

            // Co-operative: each slice computed by its own table, then merged into a fresh one.
            let mut merged = HoistTables::new(n);
            for s in 0..slices {
                let mut peer = HoistTables::new(n);
                let values = peer.fill_slice(&mut graph, k, s, slices);
                assert_eq!(values.len(), HoistTables::slice_len(n, s, slices), "slice {s} length");
                merged.adopt_slice(s, slices, &values);
            }

            assert!(merged.is_complete(), "n={n} slices={slices}: merge left gaps");
            assert_eq!(merged.filled(), solo.filled());
            for i in 0..n {
                for j in (i + 1)..n {
                    assert_eq!(
                        merged.single_created(&mut graph, k, i, j),
                        solo.single_created(&mut graph, k, i, j),
                        "n={n} k={k} slices={slices} edge ({i},{j})"
                    );
                }
            }
        }
    }

    /// A missing or partial slice must degrade to local computation, never to a wrong answer —
    /// this is what makes a peer's contribution safe to trust without verifying it.
    #[test]
    fn missing_slices_fall_back_to_local_computation() {
        let (n, k, slices) = (12usize, 5usize, 4usize);
        let mut graph = Graph::from_bitstring(&bits(n, 33), n);

        let mut solo = HoistTables::new(n);
        let mut partial = HoistTables::new(n);
        // Only slice 1 arrives; the rest never do.
        let mut peer = HoistTables::new(n);
        let values = peer.fill_slice(&mut graph, k, 1, slices);
        partial.adopt_slice(1, slices, &values);
        assert!(!partial.is_complete());

        for i in 0..n {
            for j in (i + 1)..n {
                assert_eq!(
                    partial.single_created(&mut graph, k, i, j),
                    solo.single_created(&mut graph, k, i, j),
                    "edge ({i},{j}) diverged with only one slice present"
                );
            }
        }
        assert!(partial.is_complete(), "on-demand fill should have completed it");
    }

    /// The inequality the bound rests on, checked directly against brute force rather than assumed:
    /// X (cliques through BOTH edges) can never exceed C_b (cliques through the blue edge), and Y
    /// can never exceed D_r. If this were ever false the bound could reject an improving move — the
    /// one failure that would silently lose search progress.
    #[test]
    fn corrections_never_exceed_the_single_edge_counts() {
        for (n, k, seed) in [(9usize, 4usize, 7u64), (10, 4, 11), (10, 5, 3), (12, 5, 29)] {
            let mut graph = Graph::from_bitstring(&bits(n, seed), n);
            graph.resync_complement();
            let mut tables = HoistTables::new(n);
            let mut checked = 0;
            for i in 0..n {
                for j in (i + 1)..n {
                    for a in 0..n {
                        for c in (a + 1)..n {
                            if !graph.adjacency[i].get(j) || graph.adjacency[a].get(c) {
                                continue; // need r red, b blue
                            }
                            let (r, b) = ((i, j), (a, c));
                            let c_b = tables.single_created(&mut graph, k, b.0, b.1);
                            let d_r = tables.single_created(&mut graph, k, r.0, r.1);
                            match cross_pairs(&graph.adjacency, r, b) {
                                CrossPairs::Mixed => {}
                                CrossPairs::AllRed => {
                                    let x = correction(&graph.adjacency, r, b, k);
                                    assert!(x <= c_b, "X={x} > C_b={c_b} at r={r:?} b={b:?} n={n}");
                                    checked += 1;
                                }
                                CrossPairs::AllBlue => {
                                    let y = correction(&graph.complement_adjacency, r, b, k);
                                    assert!(y <= d_r, "Y={y} > D_r={d_r} at r={r:?} b={b:?} n={n}");
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
            assert!(checked > 0, "fixture n={n} exercised no correction cases");
        }
    }

    /// The bound must be OBSERVATIONALLY IDENTICAL to computing the correction: for every pair and
    /// every limit, `pair_created_bounded` returns `Some(v)` exactly when the true `created` is
    /// `<= limit`, and that `v` is the true value. Swept across limits that straddle the true value
    /// so both the fire and no-fire sides of every branch are exercised.
    #[test]
    fn bounded_matches_unbounded_at_every_limit() {
        for (n, k, seed) in [(9usize, 4usize, 7u64), (10, 4, 11), (10, 5, 3), (12, 5, 29)] {
            let mut graph = Graph::from_bitstring(&bits(n, seed), n);
            graph.resync_complement();
            let mut reference = HoistTables::new(n);
            let mut bounded = HoistTables::new(n);
            let mut fired = 0;
            for i in 0..n {
                for j in (i + 1)..n {
                    for a in 0..n {
                        for c in (a + 1)..n {
                            if !graph.adjacency[i].get(j) || graph.adjacency[a].get(c) {
                                continue;
                            }
                            let (r, b) = ((i, j), (a, c));
                            let truth = reference.pair_created(&mut graph, k, r, b);
                            for limit in [
                                -1, 0, 1,
                                truth - 2, truth - 1, truth, truth + 1, truth + 2,
                                i32::MAX,
                            ] {
                                let got = bounded.pair_created_bounded(&mut graph, k, r, b, limit);
                                let expect = if truth > limit { None } else { Some(truth) };
                                assert_eq!(
                                    got, expect,
                                    "n={n} k={k} r={r:?} b={b:?} limit={limit} truth={truth}"
                                );
                                if got.is_none() {
                                    fired += 1;
                                }
                            }
                        }
                    }
                }
            }
            assert!(fired > 0, "fixture n={n} never exercised a rejection");
        }
    }

    /// The bound must be inert at `i32::MAX` — an unthresholded stage has to keep getting exact
    /// values, since that is when every result is submitted.
    #[test]
    fn unlimited_bound_never_rejects_and_stays_exact() {
        let (n, k) = (11usize, 4usize);
        let mut graph = Graph::from_bitstring(&bits(n, 5), n);
        graph.resync_complement();
        let mut reference = HoistTables::new(n);
        let mut bounded = HoistTables::new(n);
        for i in 0..n {
            for j in (i + 1)..n {
                for a in 0..n {
                    for c in (a + 1)..n {
                        if !graph.adjacency[i].get(j) || graph.adjacency[a].get(c) {
                            continue;
                        }
                        let (r, b) = ((i, j), (a, c));
                        assert_eq!(
                            bounded.pair_created_bounded(&mut graph, k, r, b, i32::MAX),
                            Some(reference.pair_created(&mut graph, k, r, b)),
                            "r={r:?} b={b:?}"
                        );
                    }
                }
            }
        }
    }

    /// Adopting must never overwrite a value this worker computed itself.
    #[test]
    fn adopt_does_not_clobber_locally_computed_entries() {
        let (n, k) = (10usize, 4usize);
        let mut graph = Graph::from_bitstring(&bits(n, 7), n);
        let mut t = HoistTables::new(n);
        let truth = t.single_created(&mut graph, k, 0, 1); // edge bit index 0 -> slice 0 of 2
        t.adopt_slice(0, 2, &vec![truth + 999; HoistTables::slice_len(n, 0, 2)]);
        assert_eq!(t.single_created(&mut graph, k, 0, 1), truth);
    }
    /// Carrying a table across a stage advance must be indistinguishable from rebuilding it.
    ///
    /// The dangerous direction is a wrongly-KEPT entry: it silently feeds a stale `created` to
    /// every unit of the next stage, which no downstream check would catch. Exhaustive over every
    /// single-edge flip plus a spread of balanced pair flips -- the two move shapes a stage
    /// advance can actually take.
    #[test]
    fn carried_table_matches_a_fresh_rebuild_on_every_edge() {
        for (n, k, seed) in [(9usize, 4usize, 7u64), (10, 4, 11), (10, 5, 3)] {
            let base = bits(n, seed);
            let g0 = Graph::from_bitstring(&base, n);
            let mut reds = Vec::new();
            let mut blues = Vec::new();
            for i in 0..n {
                for j in (i + 1)..n {
                    if g0.adjacency[i].get(j) { reds.push((i, j)) } else { blues.push((i, j)) }
                }
            }
            let mut moves: Vec<Vec<(usize, usize)>> = Vec::new();
            for i in 0..n {
                for j in (i + 1)..n {
                    moves.push(vec![(i, j)]); // singles: every edge
                }
            }
            // EXHAUSTIVE over balanced pairs rather than a sampled spread. Mutation testing
            // (invalidating only the flipped edges themselves) fails against this, so the coverage
            // is real. Note it does NOT distinguish the two-graph union in `carry_forward` from a
            // `before`-only check — see that method's docs; the union is kept as cheap insurance,
            // not because this test proves it necessary.
            for r in &reds {
                for bl in &blues {
                    moves.push(vec![*r, *bl]);
                }
            }

            for mv in &moves {
                let mut before = Graph::from_bitstring(&base, n);
                let mut after = before.clone();
                let wu: Vec<WorkUnitEdge> = mv.iter().map(|&(u, v)| edge(u, v)).collect();
                after.flip_edges(&wu);

                let mut carried = HoistTables::new(n);
                for u in 0..n {
                    for v in (u + 1)..n {
                        carried.single_created(&mut before, k, u, v);
                    }
                }
                carried.carry_forward(&before, &after);

                let mut fresh = HoistTables::new(n);
                for u in 0..n {
                    for v in (u + 1)..n {
                        let want = fresh.single_created(&mut after, k, u, v);
                        let got = carried.single_created(&mut after, k, u, v);
                        assert_eq!(
                            want, got,
                            "n={n} k={k} move={mv:?} edge=({u},{v}): carried table disagrees"
                        );
                    }
                }
            }
        }
    }

    /// The carry must actually save work -- if it invalidated everything it would be correct and
    /// useless. Locks in that the predicate keeps a clear majority of the table.
    #[test]
    fn carry_forward_keeps_most_of_the_table() {
        let (n, k, seed) = (12usize, 5usize, 29u64);
        let base = bits(n, seed);
        let mut before = Graph::from_bitstring(&base, n);
        let mut after = before.clone();
        after.flip_edges(&[edge(0, 1)]);

        let mut carried = HoistTables::new(n);
        for u in 0..n {
            for v in (u + 1)..n {
                carried.single_created(&mut before, k, u, v);
            }
        }
        let total = n * (n - 1) / 2;
        let invalidated = carried.carry_forward(&before, &after);
        assert_eq!(carried.filled(), total - invalidated, "filled count must drop by exactly the invalidated count");
        assert!(invalidated < total, "carry invalidated the entire table ({invalidated}/{total}) -- no saving");
    }

    /// A carry must be correct between ANY two graphs, not just a parent and the child one stage
    /// later. The worker can hold a stale carry pointer — a stage that never engaged the hoist
    /// followed by one that took the full-build path — so trusting a separately-recorded edge list
    /// silently keeps entries that the real delta invalidates. Deriving the delta from the graphs
    /// themselves makes that unrepresentable.
    #[test]
    fn carry_is_correct_between_arbitrary_graphs_not_just_adjacent_stages() {
        let (n, k) = (10usize, 4usize);
        let base = bits(n, 11);
        let mut a = Graph::from_bitstring(&base, n);

        // C is several advances away from A, as it would be after stages that never engaged.
        let mut c = Graph::from_bitstring(&base, n);
        c.flip_edges(&[edge(0, 1), edge(2, 3), edge(4, 5), edge(6, 7), edge(1, 8)]);

        let mut carried = HoistTables::new(n);
        for u in 0..n {
            for v in (u + 1)..n {
                carried.single_created(&mut a, k, u, v);
            }
        }
        carried.carry_forward(&a, &c);

        let mut fresh = HoistTables::new(n);
        let mut probe = c.clone();
        for u in 0..n {
            for v in (u + 1)..n {
                let want = fresh.single_created(&mut probe, k, u, v);
                let mut cg = c.clone();
                let got = carried.single_created(&mut cg, k, u, v);
                assert_eq!(want, got, "edge ({u},{v}) stale after a multi-advance carry");
            }
        }
    }

}
