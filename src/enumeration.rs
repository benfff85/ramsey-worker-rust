//! Work enumeration strategies for counter-based work distribution.
//!
//! Workers claim index ranges via INCRBY and use these strategies to
//! convert indices into specific edge pairs to process.

use crate::graph::{Graph, WorkUnitEdge};

/// A unit of work: either a single edge flip or a (red, blue) pair flip.
#[derive(Clone, Debug, PartialEq)]
pub enum WorkUnit {
    SingleFlip(WorkUnitEdge),
    /// (red_edge, blue_edge)
    PairFlip(WorkUnitEdge, WorkUnitEdge),
}

/// Trait for work enumeration strategies
pub trait WorkEnumerator {
    /// Convert a work index to the corresponding work unit
    fn index_to_work_unit(&self, index: i64) -> WorkUnit;

    /// Get total number of work units
    fn total_work_units(&self) -> i64;
}

/// Edge with computed cardinality for sorting
#[derive(Clone, Debug)]
pub struct ScoredEdge {
    pub vertex_one: u16,
    pub vertex_two: u16,
    pub cardinality: i32,
}

/// BASIC enumeration: simple row-major iteration over all edge pairs.
/// For n edges total, pairs are indexed as: pair_index = red_idx * blue_count + blue_idx
pub struct BasicEnumerator {
    red_edges: Vec<ScoredEdge>,
    blue_edges: Vec<ScoredEdge>,
    total_pairs: i64,
}

impl BasicEnumerator {
    pub fn new(graph: &Graph) -> Self {
        let vertex_count = graph.vertex_count;

        // Build edge list (same for red and blue in BASIC mode - just split by color)
        let mut red_edges = Vec::new();
        let mut blue_edges = Vec::new();

        for i in 0..vertex_count {
            for j in (i + 1)..vertex_count {
                let edge = ScoredEdge {
                    vertex_one: i as u16,
                    vertex_two: j as u16,
                    cardinality: 0, // Not used in BASIC
                };
                // Check adjacency - if connected (1), it's red; else blue
                if graph.adjacency[i].get(j) {
                    red_edges.push(edge);
                } else {
                    blue_edges.push(edge);
                }
            }
        }

        let total_pairs = (red_edges.len() as i64) * (blue_edges.len() as i64);

        BasicEnumerator {
            red_edges,
            blue_edges,
            total_pairs,
        }
    }
}

impl WorkEnumerator for BasicEnumerator {
    fn index_to_work_unit(&self, index: i64) -> WorkUnit {
        let blue_count = self.blue_edges.len() as i64;
        let red_idx = (index / blue_count) as usize;
        let blue_idx = (index % blue_count) as usize;

        let red_edge = &self.red_edges[red_idx];
        let blue_edge = &self.blue_edges[blue_idx];

        WorkUnit::PairFlip(
            WorkUnitEdge {
                vertex_one: red_edge.vertex_one,
                vertex_two: red_edge.vertex_two,
            },
            WorkUnitEdge {
                vertex_one: blue_edge.vertex_one,
                vertex_two: blue_edge.vertex_two,
            },
        )
    }

    fn total_work_units(&self) -> i64 {
        self.total_pairs
    }
}

/// DUAL_EDGE_CARDINALITY enumeration: edges sorted by cardinality (descending).
/// Prioritizes high-impact edge pairs first.
pub struct DualCardinalityEnumerator {
    red_edges: Vec<ScoredEdge>,
    blue_edges: Vec<ScoredEdge>,
    total_pairs: i64,
}

impl DualCardinalityEnumerator {
    pub fn new(graph: &Graph) -> Self {
        let vertex_count = graph.vertex_count;

        // Build edge list with cardinality calculation
        let mut red_edges = Vec::new();
        let mut blue_edges = Vec::new();

        // First pass: build edge lists
        for i in 0..vertex_count {
            for j in (i + 1)..vertex_count {
                let edge = ScoredEdge {
                    vertex_one: i as u16,
                    vertex_two: j as u16,
                    cardinality: 0,
                };
                if graph.adjacency[i].get(j) {
                    red_edges.push(edge);
                } else {
                    blue_edges.push(edge);
                }
            }
        }

        // Calculate cardinality for each edge
        // Cardinality = number of same-colored edges adjacent to this edge's vertices
        Self::calculate_cardinalities(&mut red_edges, graph, true);
        Self::calculate_cardinalities(&mut blue_edges, graph, false);

        // Sort by cardinality descending
        red_edges.sort_by(|a, b| b.cardinality.cmp(&a.cardinality));
        blue_edges.sort_by(|a, b| b.cardinality.cmp(&a.cardinality));

        let total_pairs = (red_edges.len() as i64) * (blue_edges.len() as i64);

        DualCardinalityEnumerator {
            red_edges,
            blue_edges,
            total_pairs,
        }
    }

    fn calculate_cardinalities(edges: &mut [ScoredEdge], graph: &Graph, is_red: bool) {
        for edge in edges.iter_mut() {
            let v1 = edge.vertex_one as usize;
            let v2 = edge.vertex_two as usize;
            let mut cardinality = 0;

            // Count same-colored neighbors of v1
            for k in 0..graph.vertex_count {
                if k != v1 && k != v2 {
                    let is_connected = graph.adjacency[v1].get(k);
                    if is_connected == is_red {
                        cardinality += 1;
                    }
                }
            }

            // Count same-colored neighbors of v2
            for k in 0..graph.vertex_count {
                if k != v1 && k != v2 {
                    let is_connected = graph.adjacency[v2].get(k);
                    if is_connected == is_red {
                        cardinality += 1;
                    }
                }
            }

            edge.cardinality = cardinality;
        }
    }
}

impl WorkEnumerator for DualCardinalityEnumerator {
    fn index_to_work_unit(&self, index: i64) -> WorkUnit {
        let blue_count = self.blue_edges.len() as i64;
        let red_idx = (index / blue_count) as usize;
        let blue_idx = (index % blue_count) as usize;

        let red_edge = &self.red_edges[red_idx];
        let blue_edge = &self.blue_edges[blue_idx];

        WorkUnit::PairFlip(
            WorkUnitEdge {
                vertex_one: red_edge.vertex_one,
                vertex_two: red_edge.vertex_two,
            },
            WorkUnitEdge {
                vertex_one: blue_edge.vertex_one,
                vertex_two: blue_edge.vertex_two,
            },
        )
    }

    fn total_work_units(&self) -> i64 {
        self.total_pairs
    }
}

/// DUAL_EDGE_CARDINALITY_WITH_SINGLES: single flips of every edge first
/// (indices [0, S) with S = red + blue: the red block then the blue block,
/// each cardinality-descending), then the full DUAL_EDGE_CARDINALITY pair
/// space shifted by S.
///
/// Both colors are enumerated, so a single-flip improvement can move
/// |red − blue| by 2 per stage advance. Balance is self-policing through the
/// objective: heavily unbalanced colorings carry more monochromatic cliques,
/// so improving flips cannot drift the balance far. (The earlier
/// majority-color-only rule kept |red − blue| ≤ 1 but locked out
/// minority-color improvers for whole stages, because pair flips preserve the
/// imbalance sign.)
pub struct DualCardinalityWithSinglesEnumerator {
    singles: Vec<ScoredEdge>,
    pairs: DualCardinalityEnumerator,
    total: i64,
}

impl DualCardinalityWithSinglesEnumerator {
    pub fn new(graph: &Graph) -> Self {
        let pairs = DualCardinalityEnumerator::new(graph);

        // Reuse the already cardinality-sorted lists from the pair enumerator:
        // red block first, matching the pair ordering convention.
        let mut singles = pairs.red_edges.clone();
        singles.extend(pairs.blue_edges.iter().cloned());

        let total = singles.len() as i64 + pairs.total_work_units();
        DualCardinalityWithSinglesEnumerator {
            singles,
            pairs,
            total,
        }
    }
}

impl WorkEnumerator for DualCardinalityWithSinglesEnumerator {
    fn index_to_work_unit(&self, index: i64) -> WorkUnit {
        let singles_count = self.singles.len() as i64;
        if index < singles_count {
            let e = &self.singles[index as usize];
            WorkUnit::SingleFlip(WorkUnitEdge {
                vertex_one: e.vertex_one,
                vertex_two: e.vertex_two,
            })
        } else {
            self.pairs.index_to_work_unit(index - singles_count)
        }
    }

    fn total_work_units(&self) -> i64 {
        self.total
    }
}

/// SEQUENTIAL_WITH_SINGLES: the same shape as DUAL_EDGE_CARDINALITY_WITH_SINGLES — every edge as a
/// single flip first (red block then blue block), then the full pair space — but in plain edge
/// order, with no cardinality scoring and no sort.
///
/// The cardinality ordering exists to sweep likely-improving moves first, which mattered when a
/// stage could only get through part of its space. It no longer does: a sweep that took ~233s now
/// takes ~6s, stages routinely reach most or all of the space, and the queue manager adopts the
/// best result in the top-50 rather than the first one found — so what order they were found in
/// does not change the outcome. Scoring cost ~564 operations per edge over 39,621 edges plus two
/// sorts, rebuilt per stage in every worker: 16.18ms each time, for an ordering whose closest
/// measured analogue (participation rank) backtested at zero signal against 4,637 real winners.
///
/// Plain edge order is also better for the hoisted path: consecutive units walk consecutive blue
/// edges, whose per-edge table entries are adjacent in memory.
pub struct SequentialWithSinglesEnumerator {
    singles: Vec<ScoredEdge>,
    pairs: BasicEnumerator,
    total: i64,
}

impl SequentialWithSinglesEnumerator {
    pub fn new(graph: &Graph) -> Self {
        let pairs = BasicEnumerator::new(graph);
        // Red block then blue block, matching the cardinality variant's convention so the only
        // difference between the two strategies is the ordering WITHIN each block.
        let mut singles = pairs.red_edges.clone();
        singles.extend(pairs.blue_edges.iter().cloned());
        let total = singles.len() as i64 + pairs.total_work_units();
        SequentialWithSinglesEnumerator { singles, pairs, total }
    }
}

impl WorkEnumerator for SequentialWithSinglesEnumerator {
    fn index_to_work_unit(&self, index: i64) -> WorkUnit {
        let singles_count = self.singles.len() as i64;
        if index < singles_count {
            let e = &self.singles[index as usize];
            WorkUnit::SingleFlip(WorkUnitEdge {
                vertex_one: e.vertex_one,
                vertex_two: e.vertex_two,
            })
        } else {
            self.pairs.index_to_work_unit(index - singles_count)
        }
    }

    fn total_work_units(&self) -> i64 {
        self.total
    }
}

/// Create the appropriate enumerator based on strategy name
pub fn create_enumerator(
    strategy: &crate::model::WorkEnumerationStrategy,
    graph: &Graph,
) -> Box<dyn WorkEnumerator + Send> {
    match strategy {
        crate::model::WorkEnumerationStrategy::BASIC => Box::new(BasicEnumerator::new(graph)),
        crate::model::WorkEnumerationStrategy::SINGLE_EDGE_CARDINALITY => {
            // For now, use same as dual (can be refined later)
            Box::new(DualCardinalityEnumerator::new(graph))
        }
        crate::model::WorkEnumerationStrategy::DUAL_EDGE_CARDINALITY => {
            Box::new(DualCardinalityEnumerator::new(graph))
        }
        crate::model::WorkEnumerationStrategy::DUAL_EDGE_CARDINALITY_WITH_SINGLES => {
            Box::new(DualCardinalityWithSinglesEnumerator::new(graph))
        }
        crate::model::WorkEnumerationStrategy::SEQUENTIAL_WITH_SINGLES => {
            Box::new(SequentialWithSinglesEnumerator::new(graph))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 6-vertex graph with mixed red/blue edges (15 edges total).
    /// Bitstring "110101101010101" → 9 red bits, 6 blue. Pair count = 9 * 6 = 54.
    const FIXTURE_BITS: &str = "110101101010101";

    /// 6-vertex fixture: 9 red, 6 blue → singles = 15 (red block then blue), pairs = 54.
    const HYBRID_FIXTURE: &str = "110101101010101";
    /// 5-vertex tie fixture (10 edges): 5 red, 5 blue → singles = 10, pairs = 25.
    const TIE_FIXTURE: &str = "1110011000";

    fn normalize_pair(a: WorkUnitEdge, b: WorkUnitEdge) -> ((u16, u16), (u16, u16)) {
        let na = if a.vertex_one < a.vertex_two {
            (a.vertex_one, a.vertex_two)
        } else {
            (a.vertex_two, a.vertex_one)
        };
        let nb = if b.vertex_one < b.vertex_two {
            (b.vertex_one, b.vertex_two)
        } else {
            (b.vertex_two, b.vertex_one)
        };
        (na, nb)
    }

    fn assert_enumerator_bijective(enumerator: &dyn WorkEnumerator) {
        let total = enumerator.total_work_units();
        assert!(total > 0);
        let mut seen: HashSet<((u16, u16), (u16, u16))> = HashSet::new();
        for i in 0..total {
            let WorkUnit::PairFlip(red, blue) = enumerator.index_to_work_unit(i) else {
                panic!("pair-only enumerator emitted a non-pair unit at index {i}");
            };
            let key = normalize_pair(red, blue);
            assert!(seen.insert(key), "duplicate at index {i}");
        }
        assert_eq!(seen.len() as i64, total, "no gaps allowed in enumeration");
    }

    #[test]
    fn basic_enumerator_index_to_work_unit_is_bijective() {
        let g = Graph::from_bitstring(FIXTURE_BITS, 6);
        assert_enumerator_bijective(&BasicEnumerator::new(&g));
    }

    #[test]
    fn dual_cardinality_enumerator_is_bijective() {
        let g = Graph::from_bitstring(FIXTURE_BITS, 6);
        assert_enumerator_bijective(&DualCardinalityEnumerator::new(&g));
    }

    #[test]
    fn basic_enumerator_total_work_units_matches_red_times_blue() {
        let g = Graph::from_bitstring(FIXTURE_BITS, 6);
        let red_count: i64 = FIXTURE_BITS.chars().filter(|c| *c == '1').count() as i64;
        let blue_count: i64 = FIXTURE_BITS.chars().filter(|c| *c == '0').count() as i64;
        let enumerator = BasicEnumerator::new(&g);
        assert_eq!(enumerator.total_work_units(), red_count * blue_count);
    }

    #[test]
    fn hybrid_singles_prefix_covers_every_edge_exactly_once_red_block_first() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let e = DualCardinalityWithSinglesEnumerator::new(&g);
        let red_count: i64 = 9;
        let blue_count: i64 = 6;
        let singles_count = red_count + blue_count;
        assert_eq!(e.total_work_units(), singles_count + red_count * blue_count);

        let mut seen: HashSet<(u16, u16)> = HashSet::new();
        for i in 0..singles_count {
            match e.index_to_work_unit(i) {
                WorkUnit::SingleFlip(edge) => {
                    let is_red =
                        g.adjacency[edge.vertex_one as usize].get(edge.vertex_two as usize);
                    if i < red_count {
                        assert!(is_red, "red block must come first (index {i})");
                    } else {
                        assert!(!is_red, "blue block must follow red block (index {i})");
                    }
                    let key = if edge.vertex_one < edge.vertex_two {
                        (edge.vertex_one, edge.vertex_two)
                    } else {
                        (edge.vertex_two, edge.vertex_one)
                    };
                    assert!(seen.insert(key), "duplicate single at index {i}");
                }
                other => panic!("expected SingleFlip at index {i}, got {other:?}"),
            }
        }
        // Every edge of the graph appears exactly once across both blocks.
        assert_eq!(seen.len() as i64, singles_count);
    }

    #[test]
    fn hybrid_pair_region_matches_plain_dual_enumerator() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let hybrid = DualCardinalityWithSinglesEnumerator::new(&g);
        let plain = DualCardinalityEnumerator::new(&g);
        let singles_count: i64 = 15; // 9 red + 6 blue
        for k in 0..plain.total_work_units() {
            assert_eq!(
                hybrid.index_to_work_unit(singles_count + k),
                plain.index_to_work_unit(k),
                "pair region diverges at offset {k}"
            );
        }
    }

    #[test]
    fn hybrid_singles_blocks_are_sorted_by_cardinality_descending() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let e = DualCardinalityWithSinglesEnumerator::new(&g);

        // Cardinality is recomputed independently per color: same-colored
        // neighbors of both endpoints, excluding the edge's own vertices.
        let cardinality = |edge: &WorkUnitEdge, is_red: bool| -> i32 {
            let (v1, v2) = (edge.vertex_one as usize, edge.vertex_two as usize);
            let mut c = 0;
            for endpoint in [v1, v2] {
                for k in 0..g.vertex_count {
                    if k != v1 && k != v2 && g.adjacency[endpoint].get(k) == is_red {
                        c += 1;
                    }
                }
            }
            c
        };

        // Ordering is descending within each color block independently; the
        // boundary between the red block (0..9) and blue block (9..15) resets.
        for (range, is_red) in [(0..9, true), (9..15, false)] {
            let mut prev = i32::MAX;
            for i in range {
                let WorkUnit::SingleFlip(edge) = e.index_to_work_unit(i) else {
                    panic!("expected single at {i}");
                };
                let c = cardinality(&edge, is_red);
                assert!(c <= prev, "cardinality increased at index {i}");
                prev = c;
            }
        }
    }

    #[test]
    fn hybrid_enumerates_both_colors_when_tied() {
        let g = Graph::from_bitstring(TIE_FIXTURE, 5);
        let e = DualCardinalityWithSinglesEnumerator::new(&g);
        // 5 red + 5 blue → 10 singles, then 25 pairs.
        assert_eq!(e.total_work_units(), 10 + 25);
        for i in 0..10 {
            let WorkUnit::SingleFlip(edge) = e.index_to_work_unit(i) else {
                panic!("expected single at {i}");
            };
            let is_red = g.adjacency[edge.vertex_one as usize].get(edge.vertex_two as usize);
            if i < 5 {
                assert!(is_red, "red block must come first (index {i})");
            } else {
                assert!(!is_red, "blue block must follow red block (index {i})");
            }
        }
    }

    // ---------- SEQUENTIAL_WITH_SINGLES ----------

    #[test]
    fn sequential_enumerator_is_bijective_over_the_pair_region() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let e = SequentialWithSinglesEnumerator::new(&g);
        let singles_count: i64 = 15;
        let mut seen: HashSet<((u16, u16), (u16, u16))> = HashSet::new();
        for i in singles_count..e.total_work_units() {
            let WorkUnit::PairFlip(red, blue) = e.index_to_work_unit(i) else {
                panic!("expected a pair at {i}");
            };
            assert!(seen.insert(normalize_pair(red, blue)), "duplicate at {i}");
        }
        assert_eq!(seen.len() as i64, e.total_work_units() - singles_count);
    }

    /// The work space must be identical to the cardinality variant's — same singles prefix, same
    /// pair count — because the queue manager computes totalPairs from the strategy NAME and the
    /// worker refuses a stage whose totals disagree.
    #[test]
    fn sequential_covers_exactly_the_same_space_as_the_cardinality_variant() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let seq = SequentialWithSinglesEnumerator::new(&g);
        let card = DualCardinalityWithSinglesEnumerator::new(&g);
        assert_eq!(seq.total_work_units(), card.total_work_units());

        let collect = |e: &dyn WorkEnumerator| {
            let mut singles = HashSet::new();
            let mut pairs = HashSet::new();
            for i in 0..e.total_work_units() {
                match e.index_to_work_unit(i) {
                    WorkUnit::SingleFlip(x) => {
                        singles.insert(if x.vertex_one < x.vertex_two {
                            (x.vertex_one, x.vertex_two)
                        } else {
                            (x.vertex_two, x.vertex_one)
                        });
                    }
                    WorkUnit::PairFlip(r, b) => {
                        pairs.insert(normalize_pair(r, b));
                    }
                }
            }
            (singles, pairs)
        };
        assert_eq!(collect(&seq), collect(&card), "same moves, different order");
    }

    /// Singles keep the red-block-then-blue-block convention; only the order within changes.
    #[test]
    fn sequential_singles_are_red_block_then_blue_block() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let e = SequentialWithSinglesEnumerator::new(&g);
        for i in 0..15i64 {
            let WorkUnit::SingleFlip(edge) = e.index_to_work_unit(i) else {
                panic!("expected a single at {i}");
            };
            let is_red = g.adjacency[edge.vertex_one as usize].get(edge.vertex_two as usize);
            assert_eq!(is_red, i < 9, "red block must come first (index {i})");
        }
    }

    /// Plain edge order, i.e. NOT sorted by cardinality — this is the whole point of the strategy.
    #[test]
    fn sequential_does_not_sort_by_cardinality() {
        let g = Graph::from_bitstring(FIXTURE_BITS, 6);
        let e = SequentialWithSinglesEnumerator::new(&g);
        // Pair index 0 must be (first red edge in edge order, first blue edge in edge order).
        let mut first_red = None;
        let mut first_blue = None;
        for i in 0..6u16 {
            for j in (i + 1)..6u16 {
                let red = g.adjacency[i as usize].get(j as usize);
                if red && first_red.is_none() { first_red = Some((i, j)) }
                if !red && first_blue.is_none() { first_blue = Some((i, j)) }
            }
        }
        let WorkUnit::PairFlip(r, b) = e.index_to_work_unit(15) else { panic!() };
        assert_eq!(
            (normalize_pair(r, b)),
            (first_red.unwrap(), first_blue.unwrap()),
            "first pair should be the first red and first blue edge in plain edge order"
        );
    }
}
