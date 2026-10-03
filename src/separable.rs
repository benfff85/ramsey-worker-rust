//! Exact row-level selection for sequential red/blue pair enumeration.
//!
//! The selector identifies blue partners that still require the existing evaluator once a
//! best-novel threshold exists. All other pairs are logical retirements with a result already
//! proven unable to beat that threshold.

use crate::bitset::{BitMatrix, BITSET_SIZE};
use crate::clique_collection::CliqueCollection;
use crate::graph::{Graph, WorkUnitEdge};
use crate::hoist::{cross_pairs, CrossPairs, HoistTables};

const NO_BLUE_EDGE: usize = usize::MAX;

#[derive(Clone, Copy, Debug)]
struct EdgeTerm {
    edge: (usize, usize),
    broken: i32,
    delta: i32,
}

/// Immutable metadata for the sequential red/blue pair ordering of one graph.
pub struct SeparableRowPlan {
    vertex_count: usize,
    base_total: i32,
    red: Vec<EdgeTerm>,
    blue: Vec<EdgeTerm>,
    blue_by_delta: Vec<usize>,
    blue_index: Vec<usize>,
    blue_incident: Vec<Vec<usize>>,
}

/// Reusable scratch state for selecting the evaluated partners of one red row.
pub struct RowSelectorScratch {
    marks: Vec<u32>,
    stamp: u32,
    selected: Vec<usize>,
}

impl RowSelectorScratch {
    pub fn new(blue_len: usize) -> Self {
        Self {
            marks: vec![0; blue_len],
            stamp: 0,
            selected: Vec::with_capacity(blue_len / 8),
        }
    }

    fn begin(&mut self) {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.marks.fill(0);
            self.stamp = 1;
        }
        self.selected.clear();
    }

    fn add(&mut self, blue_index: usize) {
        if self.marks[blue_index] != self.stamp {
            self.marks[blue_index] = self.stamp;
            self.selected.push(blue_index);
        }
    }

    pub fn contains(&self, blue_index: usize) -> bool {
        self.marks[blue_index] == self.stamp
    }

    pub fn selected(&self) -> &[usize] {
        &self.selected
    }
}

impl SeparableRowPlan {
    pub fn build(
        graph: &mut Graph,
        clique_size: usize,
        collection: &CliqueCollection,
        tables: &mut HoistTables,
    ) -> Self {
        let vertex_count = graph.vertex_count;
        let mut red_edges = Vec::new();
        let mut blue_edges = Vec::new();
        let mut blue_index = vec![NO_BLUE_EDGE; vertex_count * vertex_count];
        let mut blue_incident = vec![Vec::new(); vertex_count];

        for u in 0..vertex_count {
            for v in (u + 1)..vertex_count {
                if graph.adjacency[u].get(v) {
                    red_edges.push((u, v));
                } else {
                    let index = blue_edges.len();
                    blue_edges.push((u, v));
                    blue_index[u * vertex_count + v] = index;
                    blue_index[v * vertex_count + u] = index;
                    blue_incident[u].push(index);
                    blue_incident[v].push(index);
                }
            }
        }

        let edge_broken = |edge: (usize, usize)| {
            collection.get_count_of_cliques_containing_edges(&[WorkUnitEdge {
                vertex_one: edge.0 as u16,
                vertex_two: edge.1 as u16,
            }])
        };
        let mut red = Vec::with_capacity(red_edges.len());
        for edge in red_edges {
            let created = tables.single_created(graph, clique_size, edge.0, edge.1);
            let broken = edge_broken(edge);
            red.push(EdgeTerm {
                edge,
                broken,
                delta: created - broken,
            });
        }
        let mut blue = Vec::with_capacity(blue_edges.len());
        for edge in blue_edges {
            let created = tables.single_created(graph, clique_size, edge.0, edge.1);
            let broken = edge_broken(edge);
            blue.push(EdgeTerm {
                edge,
                broken,
                delta: created - broken,
            });
        }
        let mut blue_by_delta: Vec<usize> = (0..blue.len()).collect();
        blue_by_delta.sort_unstable_by_key(|&index| (blue[index].delta, index));

        Self {
            vertex_count,
            base_total: collection.total() as i32,
            red,
            blue,
            blue_by_delta,
            blue_index,
            blue_incident,
        }
    }

    pub fn red_len(&self) -> usize {
        self.red.len()
    }

    pub fn blue_len(&self) -> usize {
        self.blue.len()
    }

    #[inline]
    pub fn singles_len(&self) -> i64 {
        (self.red.len() + self.blue.len()) as i64
    }

    pub fn complete_row_starting_at(
        &self,
        absolute_index: i64,
        range_end: i64,
    ) -> Option<usize> {
        let blue_len = self.blue.len() as i64;
        if blue_len == 0 {
            return None;
        }
        let pair_start = self.singles_len();
        if absolute_index < pair_start || absolute_index >= range_end {
            return None;
        }
        let pair_offset = absolute_index - pair_start;
        if pair_offset % blue_len != 0 || absolute_index + blue_len > range_end {
            return None;
        }
        let red_index = (pair_offset / blue_len) as usize;
        (red_index < self.red.len()).then_some(red_index)
    }

    pub fn red_edge(&self, red_index: usize) -> (usize, usize) {
        self.red[red_index].edge
    }

    pub fn blue_edge(&self, blue_index: usize) -> (usize, usize) {
        self.blue[blue_index].edge
    }

    #[inline]
    pub fn exact_mixed_score(&self, red_index: usize, blue_index: usize) -> i32 {
        self.base_total + self.red[red_index].delta + self.blue[blue_index].delta
    }

    pub fn select_row<'a>(
        &self,
        graph: &Graph,
        red_index: usize,
        threshold: i32,
        scratch: &'a mut RowSelectorScratch,
    ) -> &'a [usize] {
        debug_assert_eq!(graph.vertex_count, self.vertex_count);
        let red = self.red[red_index];
        scratch.begin();

        let blue_delta_limit = threshold - self.base_total - red.delta;
        let candidate_end = self
            .blue_by_delta
            .partition_point(|&index| self.blue[index].delta < blue_delta_limit);
        for &blue_index in &self.blue_by_delta[..candidate_end] {
            scratch.add(blue_index);
        }

        let (x, y) = red.edge;
        let mut all_red_vertices = graph.adjacency[x];
        all_red_vertices.and_assign(&graph.adjacency[y]);
        self.add_slow_blue_edges_induced_by(
            &all_red_vertices,
            red_index,
            threshold,
            CrossPairs::AllRed,
            scratch,
        );

        let mut all_blue_vertices = graph.complement_adjacency[x];
        all_blue_vertices.and_assign(&graph.complement_adjacency[y]);
        self.add_slow_blue_edges_induced_by(
            &all_blue_vertices,
            red_index,
            threshold,
            CrossPairs::AllBlue,
            scratch,
        );

        for &blue_index in &self.blue_incident[x] {
            let relation = cross_pairs(&graph.adjacency, red.edge, self.blue[blue_index].edge);
            if self.may_beat_after_lower_bound(red_index, blue_index, threshold, relation) {
                scratch.add(blue_index);
            }
        }
        for &blue_index in &self.blue_incident[y] {
            let relation = cross_pairs(&graph.adjacency, red.edge, self.blue[blue_index].edge);
            if self.may_beat_after_lower_bound(red_index, blue_index, threshold, relation) {
                scratch.add(blue_index);
            }
        }
        &scratch.selected
    }

    #[inline]
    fn may_beat_after_lower_bound(
        &self,
        red_index: usize,
        blue_index: usize,
        threshold: i32,
        relation: CrossPairs,
    ) -> bool {
        match relation {
            CrossPairs::AllRed => {
                self.base_total + self.red[red_index].delta - self.blue[blue_index].broken
                    < threshold
            }
            CrossPairs::AllBlue => {
                self.base_total - self.red[red_index].broken + self.blue[blue_index].delta
                    < threshold
            }
            CrossPairs::Mixed => self.exact_mixed_score(red_index, blue_index) < threshold,
        }
    }

    fn add_slow_blue_edges_induced_by(
        &self,
        vertex_set: &BitMatrix,
        red_index: usize,
        threshold: i32,
        relation: CrossPairs,
        scratch: &mut RowSelectorScratch,
    ) {
        let mut vertices = [0usize; BITSET_SIZE];
        let mut count = 0usize;
        for vertex in vertex_set.iter_set_bits() {
            if vertex < self.vertex_count {
                vertices[count] = vertex;
                count += 1;
            }
        }
        for offset in 0..count {
            let u = vertices[offset];
            for &v in &vertices[(offset + 1)..count] {
                let blue_index = self.blue_index[u * self.vertex_count + v];
                if blue_index != NO_BLUE_EDGE
                    && self.may_beat_after_lower_bound(
                        red_index,
                        blue_index,
                        threshold,
                        relation,
                    )
                {
                    scratch.add(blue_index);
                }
            }
        }
    }
}
