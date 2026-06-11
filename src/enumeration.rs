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

/// DUAL_EDGE_CARDINALITY_WITH_SINGLES: single flips of the majority color
/// first (indices [0, S), cardinality-descending), then the full
/// DUAL_EDGE_CARDINALITY pair space shifted by S.
///
/// Majority-color-only singles keep |red − blue| ∈ {0, 1} across stage
/// advancement: flipping a majority edge flips the sign of the imbalance but
/// never grows it. When red and blue are exactly tied, both colors are
/// enumerated (red block first, matching the pair ordering convention).
pub struct DualCardinalityWithSinglesEnumerator {
    singles: Vec<ScoredEdge>,
    pairs: DualCardinalityEnumerator,
    total: i64,
}

impl DualCardinalityWithSinglesEnumerator {
    pub fn new(graph: &Graph) -> Self {
        let pairs = DualCardinalityEnumerator::new(graph);
        let red_len = pairs.red_edges.len();
        let blue_len = pairs.blue_edges.len();

        // Reuse the already cardinality-sorted lists from the pair enumerator.
        let singles: Vec<ScoredEdge> = if red_len > blue_len {
            pairs.red_edges.clone()
        } else if blue_len > red_len {
            pairs.blue_edges.clone()
        } else {
            let mut both = pairs.red_edges.clone();
            both.extend(pairs.blue_edges.iter().cloned());
            both
        };

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// 6-vertex graph with mixed red/blue edges (15 edges total).
    /// Bitstring "110101101010101" → 9 red bits, 6 blue. Pair count = 9 * 6 = 54.
    const FIXTURE_BITS: &str = "110101101010101";

    /// 6-vertex fixture: 9 red, 6 blue → majority red, singles = 9, pairs = 54.
    const HYBRID_FIXTURE: &str = "110101101010101";
    /// 5-vertex tie fixture (10 edges): 5 red, 5 blue → singles = 10 (both colors), pairs = 25.
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
    fn hybrid_singles_prefix_is_majority_color_each_exactly_once() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let e = DualCardinalityWithSinglesEnumerator::new(&g);
        let singles_count: i64 = 9; // red majority (9 red, 6 blue)
        assert_eq!(e.total_work_units(), singles_count + 9 * 6);

        let mut seen: HashSet<(u16, u16)> = HashSet::new();
        for i in 0..singles_count {
            match e.index_to_work_unit(i) {
                WorkUnit::SingleFlip(edge) => {
                    // Majority color is red: every single must be a red edge.
                    assert!(
                        g.adjacency[edge.vertex_one as usize].get(edge.vertex_two as usize),
                        "single at index {i} is not red"
                    );
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
        assert_eq!(seen.len() as i64, singles_count);
    }

    #[test]
    fn hybrid_pair_region_matches_plain_dual_enumerator() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let hybrid = DualCardinalityWithSinglesEnumerator::new(&g);
        let plain = DualCardinalityEnumerator::new(&g);
        let singles_count: i64 = 9; // red majority (9 red, 6 blue)
        for k in 0..plain.total_work_units() {
            assert_eq!(
                hybrid.index_to_work_unit(singles_count + k),
                plain.index_to_work_unit(k),
                "pair region diverges at offset {k}"
            );
        }
    }

    #[test]
    fn hybrid_singles_are_sorted_by_cardinality_descending() {
        let g = Graph::from_bitstring(HYBRID_FIXTURE, 6);
        let e = DualCardinalityWithSinglesEnumerator::new(&g);
        let mut prev = i32::MAX;
        for i in 0..9 {
            // 9 red majority singles
            let WorkUnit::SingleFlip(edge) = e.index_to_work_unit(i) else {
                panic!("expected single at {i}");
            };
            // Recompute cardinality independently (red edge: same-colored
            // neighbors of both endpoints, excluding the edge's own vertices).
            let (v1, v2) = (edge.vertex_one as usize, edge.vertex_two as usize);
            let mut c = 0;
            for endpoint in [v1, v2] {
                for k in 0..g.vertex_count {
                    if k != v1 && k != v2 && g.adjacency[endpoint].get(k) {
                        c += 1;
                    }
                }
            }
            assert!(c <= prev, "cardinality increased at index {i}");
            prev = c;
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
                assert!(
                    is_red,
                    "tied singles must enumerate red block first (index {i})"
                );
            } else {
                assert!(!is_red, "blue block must follow red block (index {i})");
            }
        }
    }
}
